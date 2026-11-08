//! Input-to-frame latency when a 95-page public-domain PDF image export starts from the UI.

use egui_kittest::{Harness, kittest::Queryable};
use pdfcraft_ui_egui::PdfCraftApp;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{Duration, Instant};

const PAGES: usize = 95;

fn main() {
    let input = std::env::args().nth(1).expect("pinned PDF corpus path argument");
    let input_path = Path::new(&input);
    let name = input_path.file_name().expect("PDF file name").to_string_lossy().into_owned();
    let dir = std::env::temp_dir().join(format!("pdfcraft-input-latency-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create export directory");
    let export_dir = dir.clone();
    let bytes = std::fs::read(input_path).expect("read pinned PDF");
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").expect("set language");
        app.open_bytes(&name, Some(input), bytes).expect("open pinned PDF");
        app.export_dir_override = Some(export_dir.to_string_lossy().into_owned());
        app
    });
    h.run_steps(8);
    assert!(h.state_mut().execute("export.image"));
    h.run_steps(3);

    let start = Instant::now();
    h.get_by_label("Export").click();
    let latency = start.elapsed();

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        h.step();
        let exported = std::fs::read_dir(&dir).expect("read export directory").count();
        if exported == PAGES {
            break;
        }
        assert!(Instant::now() < deadline, "export timed out after writing {exported} images");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut files = std::fs::read_dir(&dir).expect("read exported images").map(|entry| entry.expect("exported file").path()).collect::<Vec<_>>();
    files.sort();
    let mut digest = Sha256::new();
    for path in files {
        digest.update(path.file_name().unwrap().to_string_lossy().as_bytes());
        digest.update(std::fs::read(path).expect("read exported image"));
    }
    println!("CRAFT_JOURNEY_NS={}", latency.as_nanos());
    println!("CRAFT_OUTPUT_SHA256={:x}", digest.finalize());
    std::fs::remove_dir_all(&dir).expect("remove exported images");
}
