use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use colored::Colorize;
use crossbeam_channel::{bounded, Sender, Receiver};

const BUFFER_SIZE: usize = 64 * 1024; // 64 KB
const MAX_DISPLAY_FILES: usize = 10; // Максимальное количество одновременно отображаемых файлов
const MAX_PATH_LENGTH: usize = 30; // Максимальная длина отображаемого пути

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
    
    // Создаем канал для управления прогресс-барами
    let (progress_sender, progress_receiver) = bounded(MAX_DISPLAY_FILES);
    let progress_sender = Arc::new(progress_sender);
    
    // Запускаем менеджер прогресс-баров
    let manager_handle = std::thread::spawn({
        let multi_progress = multi_progress.clone();
        move || progress_manager(progress_receiver, multi_progress)
    });

    if source.is_file() {
        copy_file_with_progress(source, destination, progress_sender.clone())?;
    } else if source.is_dir() {
        copy_dir_with_progress(source, destination, progress_sender.clone())?;
    } else {
        anyhow::bail!("Source path is neither a file nor a directory: {}", source.display());
    }

    // Закрываем канал и ждем завершения менеджера
    drop(progress_sender);
    manager_handle.join().expect("Progress manager panicked")?;

    Ok(())
}

// Структура для хранения информации о активном прогрессе
struct ActiveProgress {
    pb: ProgressBar,
    finished: bool,
    id: u128,
    path: String,
}

// Менеджер прогресс-баров, который ограничивает количество одновременно отображаемых
fn progress_manager(receiver: Receiver<ProgressUpdate>, multi_progress: MultiProgress) -> Result<()> {
    let mut active_bars: Vec<ActiveProgress> = Vec::new();
    
    while let Ok(update) = receiver.recv() {
        match update {
            ProgressUpdate::NewFile { path, size, id } => {
                // Если достигли лимита, удаляем самый старый завершенный прогресс-бар
                if active_bars.len() >= MAX_DISPLAY_FILES {
                    if let Some(idx) = active_bars.iter().position(|ap| ap.finished) {
                        let active_progress = active_bars.remove(idx);
                        multi_progress.remove(&active_progress.pb);
                    }
                }
                
                // Создаем новый прогресс-бар
                let pb = multi_progress.add(ProgressBar::new(size));
                let display_path = shorten_path(&path, MAX_PATH_LENGTH);
                
                pb.set_style(ProgressStyle::with_template(&format!(
                    "{{msg:{}}} {{spinner:.green}} [{{elapsed_precise}}] {{bar:40.cyan/blue}} {{bytes:>8}}/{{total_bytes:>8}} {{bytes_per_sec:>10}} {{eta:>8}}",
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
                // Обновляем прогресс для соответствующего файла
                if let Some(active_progress) = active_bars.iter_mut().find(|ap| ap.id == id) {
                    if !active_progress.finished {
                        active_progress.pb.set_position(bytes_copied);
                    }
                }
            }
            ProgressUpdate::Finished { id } => {
                // Помечаем прогресс-бар как завершенный
                if let Some(active_progress) = active_bars.iter_mut().find(|ap| ap.id == id) {
                    active_progress.finished = true;
                    let display_path = shorten_path(&active_progress.path, MAX_PATH_LENGTH);
                    active_progress.pb.finish_with_message(format!("{} {}", "✓".green(), display_path));
                    
                    // Через некоторое время удалим завершенный прогресс-бар
                    let pb = active_progress.pb.clone();
                    let path = active_progress.path.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        // Прогресс-бар будет автоматически удален при следующем добавлении нового файла
                    });
                }
            }
        }
    }
    
    // Завершаем все оставшиеся прогресс-бары
    for active_progress in active_bars {
        let display_path = shorten_path(&active_progress.path, MAX_PATH_LENGTH);
        active_progress.pb.finish_with_message(format!("{} {}", "✓".green(), display_path));
    }
    
    Ok(())
}

fn copy_dir_with_progress(
    source: &Path,
    destination: &Path,
    progress_sender: Arc<Sender<ProgressUpdate>>,
) -> Result<()> {
    // Создаем целевую директорию
    fs::create_dir_all(destination)
        .with_context(|| format!("Failed to create destination directory: {}", destination.display()))?;

    let mut handles = vec![];
    
    // Рекурсивно обходим исходную директорию
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let dest_path = destination.join(entry.file_name());

        if source_path.is_file() {
            let sender = progress_sender.clone();
            let source_str = source_path.to_string_lossy().into_owned();
            let dest_path = dest_path.clone();
            
            handles.push(std::thread::spawn(move || {
                copy_file_with_progress_single(&source_str, &dest_path, sender)
            }));
        } else if source_path.is_dir() {
            // Рекурсивно копируем поддиректорию
            copy_dir_with_progress(&source_path, &dest_path, progress_sender.clone())?;
        }
    }

    // Дожидаемся завершения всех потоков копирования файлов
    for handle in handles {
        handle.join().expect("Thread panicked")?;
    }

    Ok(())
}

fn copy_file_with_progress(
    source: &Path,
    destination: &Path,
    progress_sender: Arc<Sender<ProgressUpdate>>,
) -> Result<()> {
    let source_str = source.to_string_lossy().into_owned();
    copy_file_with_progress_single(&source_str, destination, progress_sender)
}

fn copy_file_with_progress_single(
    source: &str,
    destination: &Path,
    progress_sender: Arc<Sender<ProgressUpdate>>,
) -> Result<()> {
    let mut source_file = File::open(source)
        .with_context(|| format!("Failed to open source file: {}", source))?;
    
    let metadata = source_file.metadata()?;
    let file_size = metadata.len();
    
    // Создаем родительские директории, если нужно
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory: {}", parent.display()))?;
    }
    
    let mut dest_file = File::create(destination)
        .with_context(|| format!("Failed to create destination file: {}", destination.display()))?;

    // Генерируем уникальный ID для файла
    let file_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    
    // Отправляем информацию о новом файле
    progress_sender.send(ProgressUpdate::NewFile {
        path: source.to_string(),
        size: file_size,
        id: file_id,
    })?;

    let mut buffer = vec![0; BUFFER_SIZE];
    let mut total_copied = 0;
    let _start_time = Instant::now();

    loop {
        let bytes_read = source_file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        dest_file.write_all(&buffer[..bytes_read])?;
        total_copied += bytes_read as u64;

        // Отправляем обновление прогресса
        progress_sender.send(ProgressUpdate::Progress {
            id: file_id,
            bytes_copied: total_copied,
        })?;
    }

    // Отправляем сообщение о завершении
    progress_sender.send(ProgressUpdate::Finished { id: file_id })?;

    Ok(())
}

// Сокращает путь до указанной длины
fn shorten_path(path: &str, max_length: usize) -> String {
    if path.len() <= max_length {
        return path.to_string();
    }
    
    let parts: Vec<&str> = path.split(std::path::MAIN_SEPARATOR).collect();
    let mut result = String::new();
    
    // Пытаемся сохранить начало и конец пути
    if parts.len() >= 3 {
        let first = parts[0];
        let last = parts[parts.len() - 1];
        
        if first.len() + last.len() + 3 <= max_length {
            // Вмещаем начало и конец
            result.push_str(first);
            result.push_str("...");
            result.push_str(last);
            return result;
        }
    }
    
    // Просто обрезаем и добавляем "..."
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

// Типы сообщений для обновления прогресса
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