//! Manual probe harness: run the extractor against a real-world PDF supplied
//! via `HAIDER_PDF_PROBE` and print the outcome. A no-op without the env var,
//! so CI never depends on host files. Used to verify the extraction ladder
//! against generator populations fixtures cannot represent (Chrome/Skia,
//! cupsfilter, Acrobat object-stream files).
#[test]
fn probe_real_world_pdf() {
    let Ok(path) = std::env::var("HAIDER_PDF_PROBE") else {
        return;
    };
    let bytes = std::fs::read(&path).expect("probe pdf reads");
    match haider_pdf::inspect_pdf(&bytes) {
        Ok(meta) => println!("INSPECT OK: {meta:?}"),
        Err(error) => println!("INSPECT ERR: {error:?}"),
    }
    match haider_pdf::extract_text_bounded(&bytes) {
        Ok(text) => println!(
            "EXTRACT OK: pages={} chars={} sample={:?}",
            text.pages_extracted,
            text.text.len(),
            &text.text[..text.text.len().min(120)]
        ),
        Err(error) => println!("EXTRACT ERR: {error:?}"),
    }
}
