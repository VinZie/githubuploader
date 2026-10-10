fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // Give the .exe its icon and file details (shown in Explorer and the taskbar).
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource.set("ProductName", "GitHub Uploader");
        resource.set("FileDescription", "GitHub Uploader");
        if let Err(err) = resource.compile() {
            panic!("can't embed the icon ({err}); building on Windows needs windres on PATH");
        }
    }
}
