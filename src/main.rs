use anyhow::{Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use colored::Colorize;

const BUFFER_SIZE: usize = 64 * 1024;
const MAX_CONCURRENT_FILES: usize = 10;
const MAX_PATH_LENGTH: usize = 30;

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

    // Собираем все файлы для копирования
    let files_to_copy = collect_files(source, destination)?;
    
    if files_to_copy.is_empty() {
        println!("No files to copy");
        return Ok(());
    }

    let total_files = files_to_copy.len();
    println!("Copying {} files...", total_files);

    let multi_progress = MultiProgress::new();
    let (progress_sender, progress_receiver) = mpsc::channel();

    // Запускаем менеджер прогресс-баров в отдельном потоке
    let manager_handle = thread::spawn({
        let multi_progress = multi_progress.clone();
        move || progress_manager(progress_receiver, multi_progress, total_files)
    });

    // Распределяем файлы по рабочим потокам заранее
    let worker_files = distribute_files_to_workers(&files_to_copy, MAX_CONCURRENT_FILES);
    
    // Создаем рабочие потоки
    let mut worker_handles = Vec::new();

    for (worker_id, files_for_worker) in worker_files.into_iter().enumerate() {
        let progress_sender = progress_sender.clone();
        
        let handle = thread::spawn(move || {
            for (i, (source_path, dest_path)) in files_for_worker.into_iter().enumerate() {
                let global_file_id = calculate_global_id(i, worker_id, MAX_CONCURRENT_FILES);
                if let Err(e) = copy_item_with_progress(
                    &source_path,
                    &dest_path,
                    progress_sender.clone(),
                    global_file_id as u32,
                ) {
                    eprintln!("Worker {}: Error copying {}: {}", worker_id, source_path, e);
                }
            }
        });
        worker_handles.push(handle);
    }

    // Ждем завершения всех рабочих потоков
    for handle in worker_handles {
        handle.join().unwrap();
    }

    // Завершаем менеджер прогресс-баров
    drop(progress_sender);
    manager_handle.join().expect("Progress manager panicked")?;

    println!("{}", "Copy completed successfully!".green());
    Ok(())
}

// Распределяем файлы по рабочим потокам
fn distribute_files_to_workers(
    files: &[(String, std::path::PathBuf)], 
    total_workers: usize
) -> Vec<Vec<(String, std::path::PathBuf)>> {
    let mut result: Vec<Vec<(String, std::path::PathBuf)>> = vec![Vec::new(); total_workers];
    
    for (i, (source, dest)) in files.iter().enumerate() {
        let worker_id = i % total_workers;
        result[worker_id].push((source.clone(), dest.clone()));
    }
    
    result
}

// Вычисляем глобальный ID файла
fn calculate_global_id(local_id: usize, worker_id: usize, total_workers: usize) -> usize {
    local_id * total_workers + worker_id
}

fn collect_files(source: &Path, destination: &Path) -> Result<Vec<(String, std::path::PathBuf)>> {
    let mut files = Vec::new();
    
    if source.is_file() || source.is_symlink() {
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

        // Включаем символические ссылки в список для копирования
        if source_path.is_file() || source_path.is_symlink() {
            let source_str = source_path.to_string_lossy().into_owned();
            files.push((source_str, dest_path));
        } else if source_path.is_dir() {
            collect_files_recursive(&source_path, &dest_path, files)?;
        }
    }

    Ok(())
}

fn copy_item_with_progress(
    source: &str,
    destination: &Path,
    progress_sender: mpsc::Sender<ProgressUpdate>,
    file_id: u32,
) -> Result<()> {
    let source_path = Path::new(source);
    
    if source_path.is_symlink() {
        // Копируем символическую ссылку
        copy_symlink(source_path, destination, progress_sender, file_id)
    } else {
        // Копируем обычный файл
        copy_file_with_progress(source, destination, progress_sender, file_id)
    }
}

fn copy_symlink(
    source: &Path,
    destination: &Path,
    progress_sender: mpsc::Sender<ProgressUpdate>,
    file_id: u32,
) -> Result<()> {
    // Получаем цель символической ссылки
    let target = fs::read_link(source)
        .with_context(|| format!("Failed to read symlink: {}", source.display()))?;

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent directory: {}", parent.display()))?;
    }

    // Удаляем существующий файл/ссылку, если он есть
    let _ = std::fs::remove_file(destination);

    // Создаем новую символическую ссылку
    symlink(&target, destination)
        .with_context(|| format!("Failed to create symlink: {}", destination.display()))?;

    // Для символических ссылок отправляем фиктивный размер и сразу завершаем
    let _ = progress_sender.send(ProgressUpdate::NewFile {
        path: source.to_string_lossy().into_owned(),
        size: 1, // Фиктивный размер для прогресс-бара
        id: file_id,
    });

    let _ = progress_sender.send(ProgressUpdate::Progress {
        id: file_id,
        bytes_copied: 1,
    });

    let _ = progress_sender.send(ProgressUpdate::Finished { id: file_id });

    Ok(())
}

fn copy_file_with_progress(
    source: &str,
    destination: &Path,
    progress_sender: mpsc::Sender<ProgressUpdate>,
    file_id: u32,
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
    id: u32,
    path: String,
}

fn progress_manager(
    receiver: mpsc::Receiver<ProgressUpdate>,
    multi_progress: MultiProgress,
    total_files: usize,
) -> Result<()> {
    let mut active_bars: Vec<ActiveProgress> = Vec::new();
    let mut completed_files = 0;
    let mut bars_to_remove: Vec<ProgressBar> = Vec::new();
    
    // Главный прогресс-бар для общего прогресса
    let main_pb = multi_progress.add(ProgressBar::new(total_files as u64));
    main_pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>3}/{len:>3} files ({percent}%)")?
            .progress_chars("█▓▒░"),
    );
    main_pb.set_message("Overall progress".to_string());

    while completed_files < total_files {
        // Сначала удаляем старые прогресс-бары
        for pb in bars_to_remove.drain(..) {
            multi_progress.remove(&pb);
        }
        
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(update) => {
                match update {
                    ProgressUpdate::NewFile { path, size, id } => {
                        // Удаляем старые завершенные прогресс-бары при достижении лимита
                        if active_bars.len() >= MAX_CONCURRENT_FILES {
                            if let Some(idx) = active_bars.iter().position(|ap| ap.finished) {
                                let completed = active_bars.remove(idx);
                                bars_to_remove.push(completed.pb);
                            }
                        }
                        
                        let pb = multi_progress.add(ProgressBar::new(size));
                        let display_path = shorten_path_safe(&path, MAX_PATH_LENGTH);
                        
                        pb.set_style(ProgressStyle::with_template(&format!(
                            "{{msg:{}}} [{{elapsed_precise}}] {{bar:40.cyan/blue}} {{bytes:>8}}/{{total_bytes:>8}} {{bytes_per_sec:>10}}",
                            MAX_PATH_LENGTH
                        ))?
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
                            let display_path = shorten_path_safe(&active_progress.path, MAX_PATH_LENGTH);
                            active_progress.pb.finish_with_message(format!("{} {}", "✓".green(), display_path));
                            completed_files += 1;
                            main_pb.inc(1);
                            
                            // Помечаем прогресс-бар для удаления в следующей итерации
                            bars_to_remove.push(active_progress.pb.clone());
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Таймаут - продолжаем проверять
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Канал закрыт, выходим
                break;
            }
        }
    }
    
    main_pb.finish_with_message("All files copied successfully!".green().to_string());
    
    // Завершаем оставшиеся прогресс-бары
    for active_progress in active_bars {
        if !active_progress.finished {
            let display_path = shorten_path_safe(&active_progress.path, MAX_PATH_LENGTH);
            active_progress.pb.finish_with_message(format!("{} {}", "✓".green(), display_path));
        }
    }
    
    Ok(())
}

// Безопасная версия shorten_path для Unicode
fn shorten_path_safe(path: &str, max_length: usize) -> String {
    if path.len() <= max_length {
        return path.to_string();
    }
    
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max_length {
        return path.to_string();
    }
    
    if chars.len() >= 3 {
        if let Some(last_sep) = path.rfind(std::path::MAIN_SEPARATOR) {
            let filename = &path[last_sep + 1..];
            let filename_chars: Vec<char> = filename.chars().collect();
            
            if filename_chars.len() + 3 <= max_length {
                return format!("...{}", filename);
            }
        }
        
        let start_chars = max_length / 2;
        let end_chars = max_length - start_chars - 3;
        
        if start_chars > 0 && end_chars > 0 {
            let start: String = chars[..start_chars].iter().collect();
            let end: String = chars[chars.len() - end_chars..].iter().collect();
            return format!("{}...{}", start, end);
        }
    }
    
    if max_length > 3 {
        let end: String = chars[chars.len() - (max_length - 3)..].iter().collect();
        format!("...{}", end)
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
        id: u32,
    },
    Progress {
        id: u32,
        bytes_copied: u64,
    },
    Finished {
        id: u32,
    },
}