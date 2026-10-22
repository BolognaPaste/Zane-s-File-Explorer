Zane's File Explorer (Tauri + Rust)

One time:   powershell -ExecutionPolicy Bypass -File setup.ps1     (admin terminal)
            then open a NEW terminal in this folder:  rustup default stable-msvc
Install:    npm install
Run:        npm run dev        (first run compiles Rust, takes several minutes)
Build exe:  npm run build      (installer ends up in src-tauri\target\release\bundle\nsis)
