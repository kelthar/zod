---
title: Icon Themes
description: "Icon Themes for Zed extensions."
---

# Icon Themes

Extensions may provide icon themes to change the icons Zed uses for folders and files.

## Example extension

The [Material Icon Theme](https://github.com/zed-extensions/material-icon-theme) serves as an example for the structure of an extension containing an icon theme.

## Directory structure

There are two important directories for an icon theme extension:

- `icon_themes`: This directory will contain one or more JSON files containing the icon theme definitions.
- `icons`: This directory contains the icon assets distributed with the extension. You can create subdirectories in this directory as needed.

Each icon theme file should adhere to the JSON schema specified at [`https://zed.dev/schema/icon_themes/v0.3.0.json`](https://zed.dev/schema/icon_themes/v0.3.0.json).

Here is an example icon theme structure:

```json [icon-theme]
{
  "$schema": "https://zed.dev/schema/icon_themes/v0.3.0.json",
  "name": "My Icon Theme",
  "author": "Your Name",
  "themes": [
    {
      "name": "My Icon Theme",
      "appearance": "dark",
      "directory_icons": {
        "collapsed": "./icons/folder.svg",
        "expanded": "./icons/folder-open.svg"
      },
      "named_directory_icons": {
        "stylesheets": {
          "collapsed": "./icons/folder-stylesheets.svg",
          "expanded": "./icons/folder-stylesheets-open.svg"
        }
      },
      "directory_content_icons": [
        {
          "pattern": "*.csproj",
          "collapsed": "./icons/folder-csharp.svg",
          "expanded": "./icons/folder-csharp-open.svg"
        }
      ],
      "chevron_icons": {
        "collapsed": "./icons/chevron-right.svg",
        "expanded": "./icons/chevron-down.svg"
      },
      "file_stems": {
        "Makefile": "make"
      },
      "file_suffixes": {
        "mp3": "audio",
        "rs": "rust"
      },
      "file_icons": {
        "audio": { "path": "./icons/audio.svg" },
        "default": { "path": "./icons/file.svg" },
        "make": { "path": "./icons/make.svg" },
        "rust": { "path": "./icons/rust.svg" }
        // ...
      }
    }
  ]
}
```

Each icon path is resolved relative to the root of the extension directory.

## Folder icons based on contents {#directory-content-icons}

`directory_content_icons` gives a folder an icon when one of its direct children
matches a glob `pattern`, for example a folder containing a `*.csproj` file. Rules
are checked in order and the first matching rule wins. These rules take
precedence over `named_directory_icons` in the Project Panel. Other panels use
`named_directory_icons` and `directory_icons` only.

Icon theme SVGs keep their own colors, so you can use multi-colored icons.

In this example, the extension would have this structure:

```
extension.toml
icon_themes/
  my-icon-theme.json
icons/
  audio.svg
  chevron-down.svg
  chevron-right.svg
  file.svg
  folder-open.svg
  folder.svg
  rust.svg
```
