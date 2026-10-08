#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result {
    eframe::run_native(
        "WindowLink",
        eframe::NativeOptions::default(),
        Box::new(|_| Ok(Box::<windowlink_android::WindowLinkApp>::default())),
    )
}

#[cfg(target_os = "android")]
fn main() {}
