Zod is a personal fork of [Zed](https://github.com/zed-industries/zed), the high-performance code editor
by Zed Industries. The aim is to make Zed better suited for .NET / C# development, with a workflow closer
to Visual Studio and Rider: run, debug and test .NET projects without hand-written `tasks.json` /
`debug.json`, and quicker navigation in large C# solutions.

Zod is not affiliated with or endorsed by Zed Industries. It shares Zed's configuration and data
directories (`~/.config/zed`), so settings, keymaps and extensions apply to both.

## Changes compared to Zed

### Branding and building

- The app is called **Zod** and the executable is `zod`, so it can run next to a regular Zed install.
- `script/bundle-zod` builds a release and installs `~/Applications/Zod.app`. It uses the
  `gpui_platform/runtime_shaders` feature, so a full Xcode installation is not required.

### Symbol search

- The project symbol picker has **Classes** and **All** modes; press <kbd>Tab</kbd> to switch.
- A space acts as a wildcard: `mer ser` matches `MercuriusService`.
- The C# language server (Roslyn) is started as soon as a project containing `.cs` files is opened,
  so symbol search works before any C# file has been opened.

### File templates

- **New File from Template** (<kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>A</kbd>, or from the project panel)
  creates files from snippet-style templates defined in `filetemplates.json`, globally in
  `~/.config/zed/filetemplates.json` or per project in `.zed/filetemplates.json`.
- `${namespace}` is derived from the nearest `.csproj` (`RootNamespace` or the project name) plus the
  folder path, e.g. `Root/Folder1/Folder2` becomes `Project.Folder1.Folder2`.

### Project panel icons

- Icon themes can set `directory_content_icons`, giving a folder its own icon when it contains a file
  matching a pattern, for example a folder containing a `*.csproj`.

### Run, debug and test (F5)

- The F5 dialog automatically discovers .NET (Core) projects in the open worktrees by evaluating the
  `.csproj` files with `dotnet msbuild` (so `Directory.Build.props` is respected) and reading
  `Properties/launchSettings.json`. .NET Framework projects are ignored.
  - **Run** tab: `dotnet run` for each runnable project and launch profile.
  - **Debug** tab: a netcoredbg debug session for each runnable project and launch profile, including
    arguments, environment variables, `applicationUrl` and working directory. The project is built first.
  - **Tests** tab (new): `dotnet test` for the whole solution or for an individual test project.
- <kbd>Tab</kbd> / <kbd>Shift</kbd>+<kbd>Tab</kbd> and <kbd>Cmd</kbd>+<kbd>1</kbd>…<kbd>5</kbd> switch
  between the tabs of the dialog.

### Debugging with netcoredbg

- A built-in [netcoredbg](https://github.com/Samsung/netcoredbg) debug adapter for C#.
- If netcoredbg is not installed, Zod offers to download and verify version 3.2.0-1092, or to open the
  releases page to install it yourself. A custom binary can be set with the `dap.netcoredbg.binary` setting.

### Test results panel

- A new **Test Results** panel opens when tests are run from the Tests tab. Results are read from the
  `.trx` files written by `dotnet test --logger trx`.
- It shows a summary (failed / passed / skipped) and lists only the failing tests. Clicking a test opens
  its source at the failing line; the disclosure arrow shows the full message and stack trace.
- Any task can feed the panel by setting the `ZOD_TEST_RESULTS_DIR` environment variable to the
  directory it writes `.trx` files to.

## Building

See [Building Zed for macOS](./docs/src/development/macos.md) for prerequisites, then run:

```sh
./script/bundle-zod
```

## Licensing

Zed, and therefore Zod, is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where
marked. See the `LICENSE-*` files for details.
