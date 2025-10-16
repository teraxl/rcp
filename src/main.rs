use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Instant;
use colored::Colorize;

const BUFFER_SIZE: usize = 64 * 1024; // 64 KB

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <source> <destination>", args[0]);
        std::process::exit(1);
    }

    let sources = &args[1..args.len() - 1];
    let destination = Path::new(&args[args.len() - 1]);

    if sources.len() > 1 && !destination.is_dir() {
        anyhow::bail!("Destination must be a directory when copying multiple files");
    }

    let multi_progress = MultiProgress::new();
    let mut handles = vec![];

    for source in sources {
        let source_path = Path::new(source);
        let dest_path = if destination.is_dir() {
            destination.join(source_path.file_name().unwrap())
        } else {
            destination.to_path_buf()
        };

        let pb = multi_progress.add(ProgressBar::new(0));
        handles.push(std::thread::spawn({
            let source = source.to_string();
            move || copy_file_with_progress(&source, &dest_path, pb)
        }));
    }

    for handle in handles {
        handle.join().expect("Thread panicked")?;
    }

    Ok(())
}

fn copy_file_with_progress(source: &str, destination: &Path, pb: ProgressBar) -> Result<()> {
    let mut source_file = File::open(source)
        .with_context(|| format!("Failed to open source file: {}", source))?;
    
    let metadata = source_file.metadata()?;
    let file_size = metadata.len();
    
    pb.set_length(file_size);
    pb.set_style(ProgressStyle::with_template(&format!(
        "{{msg:{}}} {{spinner:.green}} [{{elapsed_precise}}] {{bar:40.cyan/blue}} {{bytes:>8}}/{{total_bytes:>8}} {{bytes_per_sec:>10}} {{eta:>8}}",
        source.len().max(20)
    ))
    .unwrap()
    .with_key("bytes_per_sec", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
        write!(w, "{}/s", format_speed(state.per_sec())).unwrap()
    })
    .progress_chars("█▓▒░"));

    let mut dest_file = File::create(destination)
        .with_context(|| format!("Failed to create destination file: {}", destination.display()))?;

    let mut buffer = vec![0; BUFFER_SIZE];
    let mut total_copied = 0;
    let _start_time = Instant::now();

    pb.set_message(format!("{:20}", source.cyan().bold()));

    loop {
        let bytes_read = source_file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        dest_file.write_all(&buffer[..bytes_read])?;
        total_copied += bytes_read as u64;

        pb.set_position(total_copied);
        pb.set_message(format!("{:20}", source.cyan().bold()));
    }

    pb.finish_with_message(format!("{} {}", "✓".green(), source));
    Ok(())
}

fn format_speed(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = bytes_per_sec;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    format!("{:.1} {}", size, UNITS[unit_index])
}