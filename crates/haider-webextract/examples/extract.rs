use std::{env, fs, process};

fn main() {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: extract <html-file>");
        process::exit(2);
    };
    let html = fs::read_to_string(&path).unwrap_or_else(|error| {
        eprintln!("{path}: {error}");
        process::exit(1);
    });
    let document = haider_webextract::extract(&html, "https://doc.rust-lang.org/book/");
    println!("{}", document.markdown);
}
