use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use colored::Colorize;
use crossbeam_channel::{bounded, Sender, Receiver};
use rayon::prelude::*;

const BUFFER_SIZE: usize = 1024 * 1024 * 1024;
const MAX_CONCURRENT_FILES: usize = 4;
const MAX_PATH_LENGTH: usize = 50;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <source> <destination>", args[0]);
        std::process::exit(1);
    }

    let source = Path::new(&args[1]);
    let destination = Path::new(&args[2]);

    if !source.exists() {
        anyhow::bail!("Source path does not exist: {}", source.display());
    }

    let multi_progress = MultiProgress::new();
    let (progress_sender, progress_receiver) = bounded(100);
    let progress_sender = Arc::new(progress_sender);

    // Запускаем менеджер прогресс-баров
    let manager_handle = std::thread::spawn({
        let multi_progress = multi_progress.clone();
        move || progress_manager(progress_receiver, multi_progress)
    });

    // Собираем все файлы для копирования
    let files_to_copy = collect_files(source, destination)?;

    // Копируем файлы с использованием пула потоков rayon
    files_to_copy.into_par_iter().for_each(|(source_path, dest_path)| {
        if let Err(e) = copy_file_with_progress(&source_path, &dest_path, progress_sender.clone()) {
            eprintln!("Error copying {}: {}", source_path, e);
        }
    });

    // Завершаем менеджер прогресс-баров
    drop(progress_sender);
    manager_handle.join().expect("Progress manager panicked")?;

    Ok(())
}

fn collect_files(source: &Path, destination: &Path) -> Result<Vec<(String, std::path::PathBuf)>> {
    let mut files = Vec::new();
    
    if source.is_file() {
        let source_str = source.to_string_lossy().into_owned();
        let dest_path = if destination.is_dir() {
            destination.join(source.file_name().unwrap())
        } else {
            destination.to_path_buf()
        };
        files.push((source_str, dest_path));
    } else if source.is_dir() {
        collect_files_recursive(source, destination, &mut files)?;
    }
    
    Ok(files)
}

fn collect_files_recursive(
    source: &Path,
    destination: &Path,
    files: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("Failed to create destination directory: {}", destination.display()))?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = destination.join(entry.file_name());

        if source_path.is_symlink() {
            eprintln!("Warning: Skipping symlink: {}", source_path.display());
            continue;
        }

        if source_path.is_file() {
            let source_str = source_path.to_string_lossy().into_owned();
            files.push((source_str, dest_path));
        } else if source_path.is_dir() {
            collect_files_recursive(&source_path, &dest_path, files)?;
        }
    }

    Ok(())
}

fn copy_file_with_progress(
    source: &str,
    destination: &Path,
    progress_sender: Arc<Sender<ProgressUpdate>>,
) -> Result<()> {
    let mut source_file = File::open(source)
        .with_context(|| format!("Failed to open source file: {}", source))?;

    let metadata = source_file.metadata()?;
    let file_size = metadata.len();

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory: {}", parent.display()))?;
    }

    let mut dest_file = File::create(destination)
        .with_context(|| format!("Failed to create destination file: {}", destination.display()))?;

    let file_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    // Уведомляем о начале копирования
    let _ = progress_sender.send(ProgressUpdate::NewFile {
        path: source.to_string(),
        size: file_size,
        id: file_id,
    });

    let mut buffer = vec![0; BUFFER_SIZE];
    let mut total_copied = 0;

    loop {
        let bytes_read = match source_file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("Error reading file {}: {}", source, e);
                break;
            }
        };

        if let Err(e) = dest_file.write_all(&buffer[..bytes_read]) {
            eprintln!("Error writing file {}: {}", destination.display(), e);
            break;
        }

        total_copied += bytes_read as u64;

        // Обновляем прогресс
        let _ = progress_sender.send(ProgressUpdate::Progress {
            id: file_id,
            bytes_copied: total_copied,
        });
    }

    // Уведомляем о завершении
    let _ = progress_sender.send(ProgressUpdate::Finished { id: file_id });

    Ok(())
}

struct ActiveProgress {
    pb: ProgressBar,
    finished: bool,
    id: u128,
    path: String,
}

fn progress_manager(receiver: Receiver<ProgressUpdate>, multi_progress: MultiProgress) -> Result<()> {
    let mut active_bars: Vec<ActiveProgress> = Vec::new();
    
    while let Ok(update) = receiver.recv() {
        match update {
            ProgressUpdate::NewFile { path, size, id } => {
                // Удаляем старые завершенные прогресс-бары при достижении лимита
                if active_bars.len() >= MAX_CONCURRENT_FILES {
                    if let Some(idx) = active_bars.iter().position(|ap| ap.finished) {
                        let completed = active_bars.remove(idx);
                        multi_progress.remove(&completed.pb);
                    }
                }
                
                let pb = multi_progress.add(ProgressBar::new(size));
                let display_path = shorten_path(&path, MAX_PATH_LENGTH);
                
                pb.set_style(ProgressStyle::with_template(&format!(
                    "{{msg:{}}} [{{elapsed_precise}}] {{bar:40.cyan/blue}} {{bytes:>8}}/{{total_bytes:>8}} {{bytes_per_sec:>10}}",
                    MAX_PATH_LENGTH
                ))
                .unwrap()
                .with_key("bytes_per_sec", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    write!(w, "{}/s", format_speed(state.per_sec())).unwrap()
                })
                .progress_chars("█▓▒░"));
                
                pb.set_message(format!("{:width$}", display_path.cyan().bold(), width = MAX_PATH_LENGTH));
                
                active_bars.push(ActiveProgress {
                    pb,
                    finished: false,
                    id,
                    path,
                });
            }
            ProgressUpdate::Progress { id, bytes_copied } => {
                if let Some(active_progress) = active_bars.iter_mut().find(|ap| ap.id == id) {
                    if !active_progress.finished {
                        active_progress.pb.set_position(bytes_copied);
                    }
                }
            }
            ProgressUpdate::Finished { id } => {
                if let Some(active_progress) = active_bars.iter_mut().find(|ap| ap.id == id) {
                    active_progress.finished = true;
                    let display_path = shorten_path(&active_progress.path, MAX_PATH_LENGTH);
                    active_progress.pb.finish_with_message(format!("{} {}", "✓".green(), display_path));
                    
                    // Запланировать удаление через 2 секунды
                    let pb_to_remove = active_progress.pb.clone();
                    let multi_progress = multi_progress.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        multi_progress.remove(&pb_to_remove);
                    });
                }
            }
        }
    }
    
    // Завершаем оставшиеся прогресс-бары
    for active_progress in active_bars {
        let display_path = shorten_path(&active_progress.path, MAX_PATH_LENGTH);
        active_progress.pb.finish_with_message(format!("{} {}", "✓".green(), display_path));
    }
    
    Ok(())
}

fn shorten_path(path: &str, max_length: usize) -> String {
    if path.len() <= max_length {
        return path.to_string();
    }
    
    let parts: Vec<&str> = path.split(std::path::MAIN_SEPARATOR).collect();
    
    if parts.len() >= 3 {
        let first = parts[0];
        let last = parts[parts.len() - 1];
        
        if first.len() + last.len() + 3 <= max_length {
            return format!("{}...{}", first, last);
        }
    }
    
    if max_length > 3 {
        format!("...{}", &path[path.len() - (max_length - 3)..])
    } else {
        "...".to_string()
    }
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

#[derive(Debug)]
enum ProgressUpdate {
    NewFile {
        path: String,
        size: u64,
        id: u128,
    },
    Progress {
        id: u128,
        bytes_copied: u64,
    },
    Finished {
        id: u128,
    },
}