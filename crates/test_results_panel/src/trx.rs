//! Parser for the Visual Studio test results (`.trx`) format produced by
//! `dotnet test --logger trx`.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use collections::HashMap;
use quick_xml::{Reader, XmlVersion, events::Event};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TestRunSummary {
    pub passed: usize,
    pub skipped: usize,
    pub failures: Vec<TestFailure>,
}

impl TestRunSummary {
    pub fn total(&self) -> usize {
        self.passed + self.skipped + self.failures.len()
    }

    pub fn merge(&mut self, other: TestRunSummary) {
        self.passed += other.passed;
        self.skipped += other.skipped;
        self.failures.extend(other.failures);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestFailure {
    pub name: String,
    pub class_name: Option<String>,
    pub method_name: Option<String>,
    pub message: String,
    pub stack_trace: String,
    pub location: Option<SourceLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub path: PathBuf,
    /// One-based line number, as reported in .NET stack traces.
    pub line: u32,
}

#[derive(Default)]
struct PendingResult {
    test_id: String,
    name: String,
    outcome: String,
    message: String,
    stack_trace: String,
    has_inner_results: bool,
}

struct TestMethod {
    class_name: String,
    method_name: String,
}

#[derive(Clone, Copy)]
enum TextTarget {
    Message,
    StackTrace,
}

pub fn parse(contents: &str) -> Result<TestRunSummary> {
    let contents = contents.trim_start_matches('\u{feff}');
    let mut reader = Reader::from_str(contents);

    let mut finished_results = Vec::new();
    let mut open_results: Vec<PendingResult> = Vec::new();
    let mut methods: HashMap<String, TestMethod> = HashMap::default();
    let mut current_definition_id: Option<String> = None;
    let mut text_target: Option<TextTarget> = None;
    let mut raw_text = String::new();

    loop {
        let event = reader
            .read_event()
            .with_context(|| format!("invalid TRX at byte {}", reader.buffer_position()))?;
        let is_empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(element) | Event::Empty(element) => {
                match element.local_name().as_ref() {
                    b"UnitTestResult" => {
                        let result = PendingResult {
                            test_id: attribute(&element, "testId")?,
                            name: attribute(&element, "testName")?,
                            outcome: attribute(&element, "outcome")?,
                            ..PendingResult::default()
                        };
                        if is_empty {
                            finished_results.push(result);
                        } else {
                            open_results.push(result);
                        }
                    }
                    b"InnerResults" => {
                        if let Some(result) = open_results.last_mut() {
                            result.has_inner_results = true;
                        }
                    }
                    b"UnitTest" if !is_empty => {
                        current_definition_id = Some(attribute(&element, "id")?);
                    }
                    b"TestMethod" => {
                        if let Some(id) = current_definition_id.clone() {
                            methods.insert(
                                id,
                                TestMethod {
                                    class_name: attribute(&element, "className")?,
                                    method_name: attribute(&element, "name")?,
                                },
                            );
                        }
                    }
                    b"Message" if !is_empty && !open_results.is_empty() => {
                        text_target = Some(TextTarget::Message);
                        raw_text.clear();
                    }
                    b"StackTrace" if !is_empty && !open_results.is_empty() => {
                        text_target = Some(TextTarget::StackTrace);
                        raw_text.clear();
                    }
                    _ => {}
                }
            }
            Event::Text(text) if text_target.is_some() => {
                raw_text.push_str(&text.decode()?);
            }
            Event::CData(data) if text_target.is_some() => {
                raw_text.push_str(&quick_xml::escape::escape(data.decode()?.as_ref()));
            }
            Event::GeneralRef(reference) if text_target.is_some() => {
                raw_text.push('&');
                raw_text.push_str(&reference.decode()?);
                raw_text.push(';');
            }
            Event::End(element) => match element.local_name().as_ref() {
                b"Message" | b"StackTrace" => {
                    if let (Some(target), Some(result)) = (text_target.take(), open_results.last_mut())
                    {
                        let text = quick_xml::escape::unescape(&raw_text)
                            .map(|text| text.into_owned())
                            .unwrap_or_else(|_| raw_text.clone());
                        match target {
                            TextTarget::Message => result.message = text,
                            TextTarget::StackTrace => result.stack_trace = text,
                        }
                    }
                }
                b"UnitTestResult" => {
                    if let Some(result) = open_results.pop() {
                        // Data driven tests report a parent result whose children carry the details.
                        if !result.has_inner_results {
                            finished_results.push(result);
                        }
                    }
                }
                b"UnitTest" => current_definition_id = None,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    let mut summary = TestRunSummary::default();
    for result in finished_results {
        match result.outcome.as_str() {
            "Passed" | "PassedButRunAborted" | "Warning" => summary.passed += 1,
            "Failed" | "Error" | "Timeout" | "Aborted" => {
                let method = methods.get(&result.test_id);
                let location = method
                    .and_then(|method| {
                        location_for_method(
                            &result.stack_trace,
                            &method.class_name,
                            &method.method_name,
                        )
                    })
                    .or_else(|| stack_frames(&result.stack_trace).next());
                summary.failures.push(TestFailure {
                    name: result.name,
                    class_name: method.map(|method| method.class_name.clone()),
                    method_name: method.map(|method| method.method_name.clone()),
                    message: result.message.trim_end().to_string(),
                    stack_trace: result.stack_trace.trim_end().to_string(),
                    location,
                });
            }
            _ => summary.skipped += 1,
        }
    }
    Ok(summary)
}

fn attribute(element: &quick_xml::events::BytesStart<'_>, name: &str) -> Result<String> {
    Ok(element
        .try_get_attribute(name)?
        .map(|attribute| attribute.normalized_value(XmlVersion::Implicit1_0))
        .transpose()?
        .map(|value| value.into_owned())
        .unwrap_or_default())
}

/// Finds the stack frame belonging to the test method itself, also matching compiler generated
/// async state machines such as `Namespace.Class.<Method>d__3.MoveNext()`.
fn location_for_method(
    stack_trace: &str,
    class_name: &str,
    method_name: &str,
) -> Option<SourceLocation> {
    let class_name = class_name.replace('+', ".");
    let direct = format!("{class_name}.{method_name}(");
    let state_machine = format!("{class_name}.<{method_name}>");
    stack_trace.lines().find_map(|line| {
        let (method, location) = parse_frame(line)?;
        (method.starts_with(&direct) || method.starts_with(&state_machine)).then_some(location)
    })
}

fn stack_frames(stack_trace: &str) -> impl Iterator<Item = SourceLocation> + '_ {
    stack_trace
        .lines()
        .filter_map(|line| parse_frame(line).map(|(_, location)| location))
}

/// Parses a frame like `   at Namespace.Class.Method() in /path/File.cs:line 12`.
fn parse_frame(line: &str) -> Option<(&str, SourceLocation)> {
    let frame = line.trim_start().strip_prefix("at ")?;
    let (method, location) = frame.split_once(" in ")?;
    let (path, line_number) = location.rsplit_once(":line ")?;
    let line_number = line_number.trim().parse().ok()?;
    Some((
        method,
        SourceLocation {
            path: PathBuf::from(path),
            line: line_number,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>
<TestRun id=\"1\" xmlns=\"http://microsoft.com/schemas/VisualStudio/TeamTest/2010\">
  <Results>
    <UnitTestResult executionId=\"e1\" testId=\"t1\" testName=\"Sample.CalcTests.TheoryFails(v: 1)\" outcome=\"Passed\" />
    <UnitTestResult executionId=\"e2\" testId=\"t2\" testName=\"Sample.CalcTests.Fails\" outcome=\"Failed\">
      <Output>
        <ErrorInfo>
          <Message>Assert.Equal() Failure: Values differ
Expected: 4
Actual:   3</Message>
          <StackTrace>   at Sample.CalcTests.Fails() in /src/Sample/UnitTest1.cs:line 12
   at System.Reflection.MethodBaseInvoker.InterpretedInvoke_Method(Object obj, IntPtr* args)</StackTrace>
        </ErrorInfo>
      </Output>
    </UnitTestResult>
    <UnitTestResult executionId=\"e3\" testId=\"t3\" testName=\"Sample.CalcTests.TheoryFails(v: 2)\" outcome=\"Failed\">
      <Output>
        <ErrorInfo>
          <Message>System.InvalidOperationException : boom &lt;b&gt; &amp; \"q\"</Message>
          <StackTrace>   at Sample.Helper.Check(Int32 v) in /src/Sample/UnitTest1.cs:line 28
   at Sample.CalcTests.TheoryFails(Int32 v) in /src/Sample/UnitTest1.cs:line 18</StackTrace>
        </ErrorInfo>
      </Output>
    </UnitTestResult>
    <UnitTestResult executionId=\"e4\" testId=\"t4\" testName=\"Sample.CalcTests.Skipped\" outcome=\"NotExecuted\">
      <Output><ErrorInfo><Message>nope</Message></ErrorInfo></Output>
    </UnitTestResult>
    <UnitTestResult executionId=\"e5\" testId=\"t5\" testName=\"Sample.Nested+Inner.Async\" outcome=\"Failed\">
      <Output><ErrorInfo><Message>async failure</Message>
        <StackTrace>   at Sample.Other.Helper() in /src/Sample/Other.cs:line 3
   at Sample.Nested.Inner.&lt;Async&gt;d__0.MoveNext() in /src/Sample/Nested.cs:line 9</StackTrace>
      </ErrorInfo></Output>
    </UnitTestResult>
  </Results>
  <TestDefinitions>
    <UnitTest name=\"Sample.CalcTests.Fails\" id=\"t2\">
      <Execution id=\"e2\" />
      <TestMethod codeBase=\"Sample.dll\" className=\"Sample.CalcTests\" name=\"Fails\" />
    </UnitTest>
    <UnitTest name=\"Sample.CalcTests.TheoryFails(v: 2)\" id=\"t3\">
      <Execution id=\"e3\" />
      <TestMethod codeBase=\"Sample.dll\" className=\"Sample.CalcTests\" name=\"TheoryFails\" />
    </UnitTest>
    <UnitTest name=\"Sample.Nested+Inner.Async\" id=\"t5\">
      <Execution id=\"e5\" />
      <TestMethod codeBase=\"Sample.dll\" className=\"Sample.Nested+Inner\" name=\"Async\" />
    </UnitTest>
  </TestDefinitions>
</TestRun>";

    #[test]
    fn parses_counts_and_failures() {
        let summary = parse(SAMPLE).unwrap();
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.total(), 5);
        let names: Vec<_> = summary.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Sample.CalcTests.Fails",
                "Sample.CalcTests.TheoryFails(v: 2)",
                "Sample.Nested+Inner.Async"
            ]
        );
        let first = &summary.failures[0];
        assert_eq!(
            first.message,
            "Assert.Equal() Failure: Values differ\nExpected: 4\nActual:   3"
        );
        assert_eq!(first.method_name.as_deref(), Some("Fails"));
        assert!(first.stack_trace.starts_with("   at Sample.CalcTests.Fails()"));
        assert_eq!(
            summary.failures[1].message,
            "System.InvalidOperationException : boom <b> & \"q\""
        );
    }

    #[test]
    fn locates_the_test_method_frame() {
        let summary = parse(SAMPLE).unwrap();
        let locations: Vec<_> = summary
            .failures
            .iter()
            .map(|failure| failure.location.clone().unwrap())
            .collect();
        assert_eq!(
            locations,
            [
                SourceLocation {
                    path: "/src/Sample/UnitTest1.cs".into(),
                    line: 12
                },
                // The helper frame comes first, but the test method frame is preferred.
                SourceLocation {
                    path: "/src/Sample/UnitTest1.cs".into(),
                    line: 18
                },
                SourceLocation {
                    path: "/src/Sample/Nested.cs".into(),
                    line: 9
                },
            ]
        );
    }

    #[test]
    fn falls_back_to_first_frame_with_source() {
        let trx = "<TestRun><Results>
            <UnitTestResult testId=\"x\" testName=\"Unknown.Test\" outcome=\"Failed\">
              <Output><ErrorInfo><Message>m</Message><StackTrace>   at Lib.Code() in /src/Lib.cs:line 7
   at Unknown.Test()</StackTrace></ErrorInfo></Output>
            </UnitTestResult>
            <UnitTestResult testId=\"y\" testName=\"NoTrace\" outcome=\"Failed\" />
        </Results></TestRun>";
        let summary = parse(trx).unwrap();
        assert_eq!(
            summary.failures[0].location,
            Some(SourceLocation {
                path: "/src/Lib.cs".into(),
                line: 7
            })
        );
        assert_eq!(summary.failures[1].location, None);
        assert_eq!(summary.failures[1].message, "");
    }

    #[test]
    fn skips_parent_of_data_driven_results() {
        let trx = "<TestRun><Results>
            <UnitTestResult testId=\"p\" testName=\"Parent\" outcome=\"Failed\">
              <InnerResults>
                <UnitTestResult testId=\"p\" testName=\"Parent (1)\" outcome=\"Passed\" />
                <UnitTestResult testId=\"p\" testName=\"Parent (2)\" outcome=\"Failed\">
                  <Output><ErrorInfo><Message>row failed</Message></ErrorInfo></Output>
                </UnitTestResult>
              </InnerResults>
            </UnitTestResult>
        </Results></TestRun>";
        let summary = parse(trx).unwrap();
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.failures[0].name, "Parent (2)");
        assert_eq!(summary.failures[0].message, "row failed");
    }
}
