fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("material".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile Slint UI");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/logo/trax.ico");
        res.set("ProductName", "trax");
        res.set("FileDescription", "trax music player");
        res.set("CompanyName", "NUCL3ARN30N");
        res.set("LegalCopyright", "Copyright (C) NUCL3ARN30N, GPL-3.0");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed icon: {e}");
        }
    }
}
