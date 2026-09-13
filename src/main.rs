use chrono::Local;
use eframe::egui;
use rand::{Rng, RngCore};
use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::hash::Hasher;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

const TEST_FILE_PREFIX: &str = "test_";
const TEST_FILE_EXT: &str = ".h2w";
/// 磁盘写入时保留的剩余空间（字节），避免写满导致系统异常
const SPACE_MARGIN: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone)]
enum TaskMsg {
    Log(String),
    Progress(ProgressInfo),
    Finished,
}

#[derive(Debug, Clone, Default)]
struct ProgressInfo {
    visible: bool,
    phase: String,
    cur: u64,
    total: u64,
}

impl ProgressInfo {
    fn frac(&self) -> f32 {
        if self.total > 0 {
            (self.cur as f32 / self.total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

#[derive(Default)]
struct AppState {
    drives: Vec<String>,
    selected_drive: String,
    file_size_mb: u64,
    running: bool,
    selected_tab: usize,
    task_tx: Option<mpsc::Sender<TaskMsg>>,
    task_rx: Option<mpsc::Receiver<TaskMsg>>,
    log_queue: VecDeque<String>,
    stop_flag: Arc<Mutex<bool>>,
    progress: ProgressInfo,
}

impl AppState {
    fn new() -> Self {
        let mut s = Self::default();
        s.file_size_mb = 1024;
        s.refresh_drives();
        s
    }

    fn refresh_drives(&mut self) {
        self.drives.clear();
        for c in 'A'..='Z' {
            let p = format!("{}:\\", c);
            if Path::new(&p).exists() && c != 'C' {
                self.drives.push(p);
            }
        }
        if !self.drives.is_empty() && self.selected_drive.is_empty() {
            self.selected_drive = self.drives[0].clone();
        }
        self.log(format!("盘符刷新完成，已排除C盘：{:?}", self.drives));
    }

    fn log(&mut self, msg: String) {
        let t = Local::now().format("%H:%M:%S").to_string();
        self.log_queue.push_back(format!("[{}] {}", t, msg));
        if self.log_queue.len() > 200 {
            self.log_queue.pop_front();
        }
    }

    fn clean_test_files(&mut self) {
        let drive = &self.selected_drive;
        let mut cnt = 0;
        if let Ok(entries) = fs::read_dir(drive) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy();
                if name.starts_with(TEST_FILE_PREFIX) && name.ends_with(TEST_FILE_EXT) {
                    if fs::remove_file(entry.path()).is_ok() {
                        cnt += 1;
                    }
                }
            }
        }
        self.log(format!("清理完成，删除测试文件数量：{}", cnt));
    }

    fn clean_bench_file(&mut self) {
        let path = Path::new(&self.selected_drive).join("bench_test.tmp");
        if path.exists() {
            let _ = fs::remove_file(path);
            self.log("测速临时文件已删除".to_string());
        }
    }

    fn start_full_test(&mut self) {
        if self.running || self.selected_drive.is_empty() {
            return;
        }
        self.running = true;
        *self.stop_flag.lock().unwrap() = false;
        self.progress = ProgressInfo::default();

        let (tx, rx) = mpsc::channel();
        self.task_tx = Some(tx.clone());
        self.task_rx = Some(rx);
        let drive = self.selected_drive.clone();
        let file_size_mb = self.file_size_mb;
        let stop_flag = Arc::clone(&self.stop_flag);

        thread::spawn(move || {
            let send_log = |msg: String| {
                let _ = tx.send(TaskMsg::Log(msg));
            };
            let send_progress = |phase: &str, cur: u64, total: u64| {
                let _ = tx.send(TaskMsg::Progress(ProgressInfo {
                    visible: true,
                    phase: phase.to_string(),
                    cur,
                    total,
                }));
            };
            let gb = |b: u64| b as f64 / 1024.0 / 1024.0 / 1024.0;

            send_log("===== 全盘读写校验开始 =====".to_string());
            let file_size = file_size_mb * 1024 * 1024;
            let root = Path::new(&drive);

            let free = match free_space_bytes(root) {
                Some(s) => s,
                None => {
                    send_log("获取磁盘空间失败".to_string());
                    let _ = tx.send(TaskMsg::Finished);
                    return;
                }
            };
            send_log(format!("盘符剩余空间 {:.2} GB", gb(free)));

            // 预计可写入的总字节数（保留 SPACE_MARGIN 空间），用于进度条估算
            let est_total = free.saturating_sub(SPACE_MARGIN);
            send_progress("写入阶段", 0, est_total);

            let mut file_idx = 1u64;
            let mut total_write: u64 = 0;
            // (文件路径, 写入内容的哈希)，校验阶段据此比对
            let mut written_files: Vec<(PathBuf, u64)> = Vec::new();

            // 写入阶段（流式生成随机数据，不占用大量内存）
            loop {
                if *stop_flag.lock().unwrap() {
                    send_log("用户终止写入阶段".to_string());
                    break;
                }
                let space = free_space_bytes(root).unwrap_or(0);
                if space < file_size + SPACE_MARGIN {
                    send_log(format!(
                        "剩余空间不足，停止写入。剩余 {:.2} GB",
                        gb(space)
                    ));
                    break;
                }
                let fname = root.join(format!("{}{}{}", TEST_FILE_PREFIX, file_idx, TEST_FILE_EXT));
                send_log(format!("写入 {:?}", fname.file_name().unwrap()));
                let mut f = File::create(&fname).unwrap();
                let mut buf = vec![0u8; 128 * 1024];
                let mut rng = rand::thread_rng();
                let mut hasher = DefaultHasher::new();
                let mut wrote: u64 = 0;
                while wrote < file_size {
                    if *stop_flag.lock().unwrap() {
                        break;
                    }
                    let to_write = std::cmp::min(buf.len() as u64, file_size - wrote);
                    rng.fill_bytes(&mut buf[0..to_write as usize]);
                    f.write_all(&buf[0..to_write as usize]).unwrap();
                    hasher.write(&buf[0..to_write as usize]);
                    wrote += to_write;
                }
                total_write += wrote;
                written_files.push((fname, hasher.finish()));
                file_idx += 1;
                send_progress(
                    &format!("写入阶段 {:.2} GB / 预计 {:.2} GB", gb(total_write), gb(est_total)),
                    total_write,
                    est_total,
                );
            }
            send_log(format!("写入完成，总写入 {:.2} GB", gb(total_write)));

            // 校验阶段
            send_log("----- 开始回读校验 -----".to_string());
            let mut err_cnt = 0;
            let mut verified_bytes: u64 = 0;
            for (fp, expected_hash) in &written_files {
                if *stop_flag.lock().unwrap() {
                    break;
                }
                send_log(format!("校验 {:?}", fp.file_name().unwrap()));
                let mut f = File::open(&fp).unwrap();
                let mut buf = vec![0u8; 128 * 1024];
                let mut hasher = DefaultHasher::new();
                loop {
                    let n = f.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    verified_bytes += n as u64;
                    hasher.write(&buf[0..n]);
                }
                send_progress(
                    &format!("校验阶段 {:.2} GB / {:.2} GB", gb(verified_bytes), gb(total_write)),
                    verified_bytes,
                    total_write,
                );
                let ok = hasher.finish() == *expected_hash;
                if ok {
                    send_log(format!("✅ {:?} 校验通过", fp.file_name().unwrap()));
                } else {
                    send_log(format!("❌ {:?} 校验失败，数据损坏", fp.file_name().unwrap()));
                    err_cnt += 1;
                }
            }

            send_log("===== 全盘测试结果 =====".to_string());
            send_log(format!("错误文件数：{}", err_cnt));
            if err_cnt == 0 {
                send_log("✅ 全部校验通过".to_string());
            } else {
                send_log("❌ 检测到数据损坏：扩容盘 / 闪存坏块".to_string());
            }
            let _ = tx.send(TaskMsg::Finished);
        });
    }

    fn stop_test(&mut self) {
        *self.stop_flag.lock().unwrap() = true;
        self.log("发送停止信号，后台任务正在退出".to_string());
    }

    fn run_benchmark(&mut self) {
        if self.running || self.selected_drive.is_empty() {
            return;
        }
        self.running = true;
        *self.stop_flag.lock().unwrap() = false;
        self.progress = ProgressInfo::default();

        let (tx, rx) = mpsc::channel();
        self.task_tx = Some(tx.clone());
        self.task_rx = Some(rx);
        let drive = self.selected_drive.clone();
        let stop_flag = Arc::clone(&self.stop_flag);

        thread::spawn(move || {
            let send_log = |msg: String| {
                let _ = tx.send(TaskMsg::Log(msg));
            };
            let send_progress = |phase: &str, cur: u64, total: u64| {
                let _ = tx.send(TaskMsg::Progress(ProgressInfo {
                    visible: true,
                    phase: phase.to_string(),
                    cur,
                    total,
                }));
            };

            send_log("==== 简易测速开始 ====".to_string());
            let tmp_path = Path::new(&drive).join("bench_test.tmp");
            let size_seq = 100 * 1024 * 1024;
            let mut buf = vec![0u8; 128 * 1024];
            let mut rng = rand::thread_rng();

            // 连续写入
            send_progress("连续写入", 0, size_seq as u64);
            let t0 = std::time::Instant::now();
            let mut f = File::create(&tmp_path).unwrap();
            let mut remain = size_seq;
            let mut chunk = 0usize;
            while remain > 0 {
                if *stop_flag.lock().unwrap() {
                    break;
                }
                let w = std::cmp::min(buf.len(), remain);
                rng.fill_bytes(&mut buf[0..w]);
                f.write_all(&buf[0..w]).unwrap();
                remain -= w;
                chunk += 1;
                if chunk % 64 == 0 {
                    send_progress("连续写入", (size_seq - remain) as u64, size_seq as u64);
                }
            }
            let t1 = t0.elapsed().as_secs_f64();
            let write_seq = size_seq as f64 / 1024.0 / 1024.0 / t1;
            send_progress("连续写入", size_seq as u64, size_seq as u64);

            // 连续读取
            send_progress("连续读取", 0, size_seq as u64);
            let t0 = std::time::Instant::now();
            let mut f = File::open(&tmp_path).unwrap();
            let mut _dst = vec![0u8; 128 * 1024];
            let mut remain = size_seq;
            chunk = 0;
            while remain > 0 {
                let n = f.read(&mut _dst).unwrap();
                if n == 0 {
                    break;
                }
                remain -= n;
                chunk += 1;
                if chunk % 64 == 0 {
                    send_progress("连续读取", (size_seq - remain) as u64, size_seq as u64);
                }
            }
            let t1 = t0.elapsed().as_secs_f64();
            let read_seq = size_seq as f64 / 1024.0 / 1024.0 / t1;
            send_progress("连续读取", size_seq as u64, size_seq as u64);
            send_log(format!("连续写入: {:.2} MB/s | 连续读取: {:.2} MB/s", write_seq, read_seq));

            // 4K随机读写
            const BLOCK: usize = 4096;
            const COUNT: usize = 20000;
            send_progress("4K随机写入", 0, COUNT as u64);
            let t0 = std::time::Instant::now();
            let mut f = OpenOptions::new().read(true).write(true).open(&tmp_path).unwrap();
            for i in 0..COUNT {
                if *stop_flag.lock().unwrap() {
                    break;
                }
                let pos = rand::thread_rng().gen_range(0..size_seq - BLOCK);
                f.seek(std::io::SeekFrom::Start(pos as u64)).unwrap();
                let mut b = [0u8; BLOCK];
                rand::thread_rng().fill_bytes(&mut b);
                f.write_all(&b).unwrap();
                if i % 500 == 0 {
                    send_progress("4K随机写入", i as u64, COUNT as u64);
                }
            }
            let t1 = t0.elapsed().as_secs_f64();
            let write_4k = (COUNT * BLOCK) as f64 / 1024.0 / 1024.0 / t1;
            send_progress("4K随机写入", COUNT as u64, COUNT as u64);

            send_progress("4K随机读取", 0, COUNT as u64);
            let t0 = std::time::Instant::now();
            let mut f = File::open(&tmp_path).unwrap();
            for i in 0..COUNT {
                if *stop_flag.lock().unwrap() {
                    break;
                }
                let pos = rand::thread_rng().gen_range(0..size_seq - BLOCK);
                f.seek(std::io::SeekFrom::Start(pos as u64)).unwrap();
                let mut b = [0u8; BLOCK];
                f.read_exact(&mut b).unwrap();
                if i % 500 == 0 {
                    send_progress("4K随机读取", i as u64, COUNT as u64);
                }
            }
            let t1 = t0.elapsed().as_secs_f64();
            let read_4k = (COUNT * BLOCK) as f64 / 1024.0 / 1024.0 / t1;
            send_progress("4K随机读取", COUNT as u64, COUNT as u64);
            send_log(format!("4K随机写入: {:.3} MB/s | 4K随机读取: {:.3} MB/s", write_4k, read_4k));

            let _ = fs::remove_file(tmp_path);
            send_log("==== 测速完成 ====".to_string());
            let _ = tx.send(TaskMsg::Finished);
        });
    }

    fn read_usb_vid_pid(&mut self) {
        if self.running {
            return;
        }
        self.running = true;
        *self.stop_flag.lock().unwrap() = false;

        let (tx, rx) = mpsc::channel();
        self.task_tx = Some(tx.clone());
        self.task_rx = Some(rx);
        thread::spawn(move || {
            let send_log = |msg: String| {
                let _ = tx.send(TaskMsg::Log(msg));
            };
            send_log("枚举USB设备VID/PID：".to_string());
            let out = std::process::Command::new("powershell")
                .args([
                    "-Command",
                    "Get-PnpDevice -PresentOnly | Where-Object {$_.InstanceId -match '^USB'} | Select-Object InstanceId, FriendlyName | Format-List",
                ])
                .output();
            match out {
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout);
                    send_log(s.to_string());
                }
                Err(e) => send_log(format!("读取USB信息失败:{}", e)),
            }
            let _ = tx.send(TaskMsg::Finished);
        });
    }

    fn poll_messages(&mut self) {
        // 先把消息全部取出来，避免循环体里再可变借用 self 造成借用冲突
        let mut msgs: Vec<TaskMsg> = Vec::new();
        if let Some(rx) = self.task_rx.as_ref() {
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
        }
        for msg in msgs {
            match msg {
                TaskMsg::Log(s) => self.log(s),
                TaskMsg::Progress(p) => self.progress = p,
                TaskMsg::Finished => {
                    self.running = false;
                    self.task_tx.take();
                    self.task_rx.take();
                    self.progress = ProgressInfo::default();
                }
            }
        }
    }
}

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("开源U盘校验测速工具（Rust版，仿H2testw）");
            ui.separator();
            ui.horizontal(|ui| {
                for (i, name) in ["全盘读写校验", "简易测速", "USB设备信息"].iter().enumerate() {
                    if ui.selectable_label(self.selected_tab == i, *name).clicked() {
                        self.selected_tab = i;
                    }
                }
            });

            match self.selected_tab {
                0 => {
                    ui.horizontal(|ui| {
                        ui.label("盘符：");
                        egui::ComboBox::new("drive_select", "")
                            .selected_text(self.selected_drive.as_str())
                            .show_ui(ui, |ui| {
                                for d in &self.drives {
                                    ui.selectable_value(&mut self.selected_drive, d.clone(), d.as_str());
                                }
                            });
                        if ui.button("刷新盘符").clicked() {
                            self.refresh_drives();
                        }
                        ui.label("单文件大小 MB：");
                        ui.add(egui::DragValue::new(&mut self.file_size_mb).clamp_range(100..=4096));
                    });
                    ui.horizontal(|ui| {
                        if ui.button("全盘写入+校验").clicked() && !self.running {
                            self.start_full_test();
                        }
                        if ui.button("停止").clicked() {
                            self.stop_test();
                        }
                        if ui.button("清理测试文件").clicked() {
                            self.clean_test_files();
                        }
                    });
                }
                1 => {
                    ui.horizontal(|ui| {
                        if ui.button("开始测速（连续+4K）").clicked() && !self.running {
                            self.run_benchmark();
                        }
                        if ui.button("清理测速临时文件").clicked() {
                            self.clean_bench_file();
                        }
                    });
                }
                2 => {
                    if ui.button("读取USB VID/PID").clicked() && !self.running {
                        self.read_usb_vid_pid();
                    }
                    ui.label("提示：拿到VID_xxxx&PID_xxxx，去ChipGenius数据库查询主控型号，无法直接读取闪存颗粒信息。");
                }
                _ => {}
            }

            ui.separator();
            if self.progress.visible {
                ui.label(self.progress.phase.as_str());
                ui.add(egui::ProgressBar::new(self.progress.frac()).show_percentage().animate(self.running));
                ui.separator();
            }
            ui.label("运行日志：");
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                for line in &self.log_queue {
                    ui.label(line.as_str());
                }
            });
        });
        ctx.request_repaint();
    }
}

/// 获取指定目录所在磁盘的剩余空间（字节）。Windows 下使用系统 API。
#[cfg(windows)]
fn free_space_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free_total: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut free_total,
        )
    };
    if ok != 0 {
        Some(free_total)
    } else {
        None
    }
}

#[cfg(not(windows))]
fn free_space_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(windows)]
#[link(name = "Kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        lp_directory_name: *const u16,
        lp_free_bytes_available_to_caller: *mut u64,
        lp_total_number_of_bytes: *mut u64,
        lp_total_number_of_free_bytes: *mut u64,
    ) -> i32;
}

fn main() -> Result<(), eframe::Error> {
    let native_options = eframe::NativeOptions {
        initial_window_size: Some(egui::vec2(820.0, 620.0)),
        ..Default::default()
    };
    eframe::run_native(
        "USB Tester Rust",
        native_options,
        Box::new(|_creation_context| Box::new(AppState::new())),
    )
}
