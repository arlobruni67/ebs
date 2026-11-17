fn main() {
    // Import libraries for the bundled FFmpeg 9 shared build (avcodec-63.dll etc.).
    let dir = std::env::var("EBS_FFMPEG_LIB").unwrap_or_else(|_| r"D:\ArloCapture\third_party\ffmpeg\lib".into());
    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rerun-if-env-changed=EBS_FFMPEG_LIB");
}
