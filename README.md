# Zane's File Explorer

A fast, two-pane file manager for Windows with a deep-space look. It takes the handiest parts of Windows File Explorer and Directory Opus and puts them in one small app.

## Download

Get the installer from the [Releases page](https://github.com/BolognaPaste/Zane-s-File-Explorer/releases) and run it.

The installer isn't code-signed, so Windows may show a SmartScreen warning. Click **More info → Run anyway**.

## Features

- **Two panes with tabs.** Copy or move between them with F5 and F6, or drag and drop.
- **Fast.** Folders with tens of thousands of files open in a moment.
- **Type to find.** Start typing to jump to a file, or filter the folder from the search box. Press Enter to search subfolders too.
- **Rename in place** with F2, or rename many files at once with find and replace and numbering.
- **Undo and redo** (Ctrl+Z / Ctrl+Y) for renames, moves, copies and deletes.
- **Drag files out** into other apps, such as a browser, chat or email.
- **Big copies show progress**, with time left and a Cancel button.
- **Thumbnails and previews** for images, video, audio, PDFs, text and Markdown.
- **Remembers things.** Your tabs, and each folder's sort order and view.
- **Two themes:** Deep space (dark) and Bright hull (light).

Every keyboard shortcut is listed in **Settings → Keyboard and mouse**.

## Build it yourself

You need Windows 10 or 11.

1. Install the tools once. In an administrator PowerShell window, in this folder, run:
   ```
   powershell -ExecutionPolicy Bypass -File setup.ps1
   ```
2. Open a new terminal in this folder and run:
   ```
   rustup default stable-msvc
   npm install
   ```
3. Start the app:
   ```
   npm run dev
   ```
   The first run compiles the Rust code and takes a few minutes.
4. Make an installer:
   ```
   npm run build
   ```
   It ends up in `src-tauri\target\release\bundle\nsis`.

## How it's made

Built with [Tauri 2](https://tauri.app). The window is a single HTML file (`src/index.html`), and the file operations are in Rust (`src-tauri/src/main.rs`).
