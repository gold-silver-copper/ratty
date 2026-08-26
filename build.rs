fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=assets/ratty.ico");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rustc-check-cfg=cfg(rio_vt_sgr_blink)");
    println!("cargo:rustc-check-cfg=cfg(bevy_terminal_automatic_metrics)");

    // Cargo removes git sources from dependencies when it creates a crates.io
    // package. Enable blink propagation only when this checkout is actually
    // using the rio-vt fork that preserves SGR 5/6/25.
    let manifest = std::fs::read_to_string("Cargo.toml")?;
    if manifest.contains("https://github.com/gold-silver-copper/rio.git")
        && manifest.contains("f36b84c6e55cad97be300414774d47fa99c1790d")
    {
        println!("cargo:rustc-cfg=rio_vt_sgr_blink");
    }

    // Cargo removes git sources from dependencies when it creates a crates.io
    // package. The git renderer derives cells directly; packaged-source builds
    // use Ratty's measured-advance adapter for the source-compatible 0.7 crate.
    if manifest.contains("https://github.com/gold-silver-copper/bevy_terminal.git")
        && manifest.contains("2192e8a93b54e09119bd790a1263373db8fec4fa")
    {
        println!("cargo:rustc-cfg=bevy_terminal_automatic_metrics");
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/ratty.ico").set_manifest(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application>
    <windowsSettings>
      <consoleAllocationPolicy xmlns="http://schemas.microsoft.com/SMI/2024/WindowsSettings">detached</consoleAllocationPolicy>
    </windowsSettings>
  </application>
</assembly>
"#,
    );

    resource.compile()
}
