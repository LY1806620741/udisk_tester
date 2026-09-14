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
    fonts_ready: bool,
}

impl AppState {
    fn new() -> Self {
        let mut s = Self::default();
        s.file_size_mb = 1024;
        s.refresh_drives();
        s
    }

    /// 加载系统中文字体作为 UI 字体的 fallback，避免中文显示为方块
    fn setup_cjk_fonts(&mut self, ctx: &egui::Context) {
        match find_cjk_font() {
            Some(path) => match std::fs::read(&path) {
                Ok(bytes) => {
                    let mut fonts = egui::FontDefinitions::default();
                    fonts
                        .font_data
                        .insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
                    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                        fonts
                            .families
                            .entry(family)
                            .or_default()
                            .push("cjk".to_owned());
                    }
                    ctx.set_fonts(fonts);
                    self.log(format!("已加载中文字体：{}", path.display()));
                }
                Err(e) => self.log(format!("中文字体读取失败 {}：{}", path.display(), e)),
            },
            None => self.log(
                "未找到中文字体，中文可能显示为方块；建议安装 fonts-noto-cjk（Linux）".to_string(),
            ),
        }
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
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_test_file(&name) {
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
            let stop_check = || *stop_flag.lock().unwrap();

            // 写入阶段（流式生成随机数据，不占用大量内存）
            loop {
                if *stop_flag.lock().unwrap() {
                    send_log("用户终止写入阶段".to_string());
                    break;
                }
                let space = free_space_bytes(root).unwrap_or(0);
                if !has_enough_space(space, file_size) {
                    send_log(format!(
                        "剩余空间不足，停止写入。剩余 {:.2} GB",
                        gb(space)
                    ));
                    break;
                }
                let fname = root.join(format!("{}{}{}", TEST_FILE_PREFIX, file_idx, TEST_FILE_EXT));
                if fname.exists() {
                    send_log(format!("⚠ {:?} 已存在，将被覆盖", fname.file_name().unwrap()));
                }
                send_log(format!("写入 {:?}", fname.file_name().unwrap()));
                let (hash, wrote) = match write_random_file(&fname, file_size, &stop_check) {
                    Ok(v) => v,
                    Err(e) => {
                        send_log(format!("写入失败 {:?}: {}", fname.file_name().unwrap(), e));
                        break;
                    }
                };
                total_write += wrote;
                written_files.push((fname, hash));
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
                let (hash, bytes) = match hash_file(fp, &stop_check) {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        send_log(format!("用户终止校验 {:?}", fp.file_name().unwrap()));
                        break;
                    }
                    Err(e) => {
                        send_log(format!("读取失败 {:?}: {}", fp.file_name().unwrap(), e));
                        err_cnt += 1;
                        continue;
                    }
                };
                verified_bytes += bytes;
                send_progress(
                    &format!("校验阶段 {:.2} GB / {:.2} GB", gb(verified_bytes), gb(total_write)),
                    verified_bytes,
                    total_write,
                );
                let ok = hash == *expected_hash;
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
            if tmp_path.exists() {
                send_log("⚠ bench_test.tmp 已存在，将被覆盖".to_string());
            }
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
            let write_seq = mb_per_sec(size_seq as u64, t1);
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
            let read_seq = mb_per_sec(size_seq as u64, t1);
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
            let write_4k = mb_per_sec((COUNT * BLOCK) as u64, t1);
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
            let read_4k = mb_per_sec((COUNT * BLOCK) as u64, t1);
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
        if !self.fonts_ready {
            self.setup_cjk_fonts(ctx);
            self.fonts_ready = true;
        }
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
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 130, 20),
                        "⚠ 全盘校验会持续写入所选盘符直至仅剩约 100MB 空间；请确认盘符无误、重要数据已备份。",
                    );
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

/// 字节数转 GiB（1024^3）
fn gb(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0 / 1024.0
}

/// 计算吞吐量 MB/s（bytes / 1024^2 / 秒）
fn mb_per_sec(bytes: u64, secs: f64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / secs
}

/// 常见系统中文字体候选路径（按优先级），用于跨平台提供 CJK 字形。
/// Linux 优先 SC 简体专版，其次 Noto CJK TTC / 文泉驿 / AR PL。
const CJK_FONT_CANDIDATES: &[&str] = &[
    // Windows
    "C:\\Windows\\Fonts\\msyh.ttc", // 微软雅黑
    "C:\\Windows\\Fonts\\msyh.ttf",
    "C:\\Windows\\Fonts\\simhei.ttf", // 黑体
    "C:\\Windows\\Fonts\\simsun.ttc", // 宋体
    // macOS
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/STHeiti Light.ttc",
    "/Library/Fonts/Arial Unicode.ttf",
    // Linux：Noto CJK SC 简体专版优先
    "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-sans-cjk-ttc/NotoSansCJK-Regular.ttc",
    // Linux：文泉驿 / Droid / AR PL
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    "/usr/share/fonts/truetype/arphic/uming.ttc",
];

/// 用户目录下常见的中文字体文件名（~/.fonts 与 ~/.local/share/fonts）
const USER_CJK_FONT_FILES: &[&str] = &[
    "NotoSansCJKsc-Regular.otf",
    "NotoSansCJK-Regular.ttc",
    "wqy-microhei.ttc",
    "wqy-zenhei.ttc",
    "SourceHanSansSC-Regular.otf",
    "msyh.ttc",
];

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// 找到第一个可读取的中文字体文件（跨平台候选 + 用户目录）
fn find_cjk_font() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = CJK_FONT_CANDIDATES.iter().map(PathBuf::from).collect();
    if let Some(home) = home_dir() {
        for f in USER_CJK_FONT_FILES {
            candidates.push(home.join(".fonts").join(f));
            candidates.push(home.join(".local").join("share").join("fonts").join(f));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// 判断文件名是否为测试文件（前缀 + 后缀匹配）
fn is_test_file(name: &str) -> bool {
    name.starts_with(TEST_FILE_PREFIX) && name.ends_with(TEST_FILE_EXT)
}

/// 剩余空间是否足够再写一个文件（写入后仍需保留 SPACE_MARGIN 余量）
fn has_enough_space(space: u64, file_size: u64) -> bool {
    space >= file_size + SPACE_MARGIN
}

/// 以 128KB 块流式写入随机数据并计算内容哈希，返回 (哈希, 实际写入字节数)。
/// 写入过程中每写一块都会检查 should_stop，用于支持用户中途停止。
fn write_random_file(path: &Path, size: u64, should_stop: impl Fn() -> bool) -> std::io::Result<(u64, u64)> {
    let mut f = File::create(path)?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut rng = rand::thread_rng();
    let mut hasher = DefaultHasher::new();
    let mut wrote: u64 = 0;
    while wrote < size {
        if should_stop() {
            break;
        }
        let to_write = std::cmp::min(buf.len() as u64, size - wrote);
        rng.fill_bytes(&mut buf[0..to_write as usize]);
        f.write_all(&buf[0..to_write as usize])?;
        hasher.write(&buf[0..to_write as usize]);
        wrote += to_write;
    }
    Ok((hasher.finish(), wrote))
}

/// 读取整个文件并计算哈希，返回 (哈希, 读取的总字节数)。
/// 每读一块都会检查 should_stop；被停止时返回 Ok(None)。
fn hash_file(path: &Path, should_stop: impl Fn() -> bool) -> std::io::Result<Option<(u64, u64)>> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut hasher = DefaultHasher::new();
    let mut total: u64 = 0;
    loop {
        if should_stop() {
            return Ok(None);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.write(&buf[0..n]);
    }
    Ok(Some((hasher.finish(), total)))
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

#[cfg(test)]
mod tests {
    use super::{
        find_cjk_font, gb, has_enough_space, hash_file, is_test_file, mb_per_sec, write_random_file,
        AppState, ProgressInfo, TaskMsg, SPACE_MARGIN,
    };
    use std::fs;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use ab_glyph::Font as _;

    /// 每个测试使用独立的临时目录，避免并行测试互相干扰
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("udisk_tester_test_{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn progress_frac_normal() {
        let p = ProgressInfo {
            visible: true,
            phase: "测试".to_string(),
            cur: 50,
            total: 200,
        };
        assert!((p.frac() - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn progress_frac_total_zero() {
        let p = ProgressInfo::default();
        assert_eq!(p.frac(), 0.0);
    }

    #[test]
    fn progress_frac_clamped() {
        let over = ProgressInfo {
            visible: true,
            phase: String::new(),
            cur: 300,
            total: 100,
        };
        assert_eq!(over.frac(), 1.0);
        let under = ProgressInfo {
            visible: true,
            phase: String::new(),
            cur: 0,
            total: 100,
        };
        assert_eq!(under.frac(), 0.0);
    }

    #[test]
    fn write_then_hash_round_trip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("t.bin");
        let (hash, wrote) = write_random_file(&path, 256 * 1024, || false).unwrap();
        assert_eq!(wrote, 256 * 1024, "应写满整个文件");
        let (hash2, bytes) = hash_file(&path, || false).unwrap().unwrap();
        assert_eq!(bytes, 256 * 1024, "回读字节数应与写入一致");
        assert_eq!(hash, hash2, "写入哈希与回读哈希应一致");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corruption_detected() {
        let dir = temp_dir("corrupt");
        let path = dir.join("t.bin");
        let (hash, _) = write_random_file(&path, 256 * 1024, || false).unwrap();
        // 翻转文件中间一个字节，模拟数据损坏（扩容盘/闪存坏块）
        let mut f = fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(128 * 1024)).unwrap();
        let mut b = [0u8; 1];
        f.read_exact(&mut b).unwrap();
        f.seek(SeekFrom::Start(128 * 1024)).unwrap();
        f.write_all(&[b[0] ^ 0xFF]).unwrap();
        drop(f);
        let (hash2, _) = hash_file(&path, || false).unwrap().unwrap();
        assert_ne!(hash, hash2, "内容被篡改后哈希必须不同");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stop_interrupts_write() {
        let dir = temp_dir("stop");
        let path = dir.join("t.bin");
        let calls = AtomicUsize::new(0);
        let should_stop = || calls.fetch_add(1, Ordering::Relaxed) >= 8;
        let (hash, wrote) = write_random_file(&path, 10 * 1024 * 1024, &should_stop).unwrap();
        assert!(wrote < 10 * 1024 * 1024, "停止后不应写满");
        assert_eq!(wrote, 8 * 128 * 1024, "应在第 8 个块后被中断");
        // 中断前写入的部分应能被完整、正确地校验
        let (hash2, bytes) = hash_file(&path, || false).unwrap().unwrap();
        assert_eq!(bytes, wrote);
        assert_eq!(hash, hash2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_file_hashes_stable() {
        let dir = temp_dir("empty");
        let path = dir.join("e.bin");
        let (h1, w) = write_random_file(&path, 0, || false).unwrap();
        assert_eq!(w, 0, "空文件不应写入任何字节");
        let (h2, b) = hash_file(&path, || false).unwrap().unwrap();
        assert_eq!(b, 0);
        assert_eq!(h1, h2, "空文件的写入哈希与回读哈希应一致");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 模拟全盘校验的"多文件写入 → 逐文件回读比对"流程
    #[test]
    fn multi_file_round_trip_all_pass() {
        let dir = temp_dir("multi");
        let mut files = Vec::new();
        for i in 0..3u64 {
            let path = dir.join(format!("f{}.bin", i));
            let (h, _) = write_random_file(&path, (i + 1) * 64 * 1024, || false).unwrap();
            files.push((path, h));
        }
        let mut errs = 0usize;
        for (p, expected) in &files {
            let (h, _) = hash_file(p, || false).unwrap().unwrap();
            if h != *expected {
                errs += 1;
            }
        }
        assert_eq!(errs, 0, "所有文件都应校验通过");
        fs::remove_dir_all(&dir).unwrap();
    }

    /// 只损坏其中一个文件时，应只报出该文件损坏
    #[test]
    fn multi_file_corruption_isolated() {
        let dir = temp_dir("multi_corrupt");
        let mut files = Vec::new();
        for i in 0..3u64 {
            let path = dir.join(format!("f{}.bin", i));
            let (h, _) = write_random_file(&path, 64 * 1024, || false).unwrap();
            files.push((path, h));
        }
        // 只损坏第二个文件的首字节
        let mut f = fs::OpenOptions::new().read(true).write(true).open(&files[1].0).unwrap();
        let mut b = [0u8; 1];
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_exact(&mut b).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[b[0] ^ 0xFF]).unwrap();
        drop(f);

        let mut errs: Vec<usize> = Vec::new();
        for (i, (p, expected)) in files.iter().enumerate() {
            let (h, _) = hash_file(p, || false).unwrap().unwrap();
            if h != *expected {
                errs.push(i);
            }
        }
        assert_eq!(errs, vec![1], "只有被损坏的第 2 个文件应报错");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mb_per_sec_math() {
        assert_eq!(mb_per_sec(0, 1.0), 0.0);
        let r = mb_per_sec(100 * 1024 * 1024, 1.0);
        assert!((r - 100.0).abs() < 1e-6, "100MB/秒 应等于 100 MB/s");
        let r2 = mb_per_sec(100 * 1024 * 1024, 0.5);
        assert!((r2 - 200.0).abs() < 1e-6, "耗时减半吞吐应翻倍");
    }

    #[test]
    fn gb_math() {
        assert_eq!(gb(0), 0.0);
        assert!((gb(1024 * 1024 * 1024) - 1.0).abs() < 1e-9);
        assert!((gb(512 * 1024 * 1024) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn is_test_file_matches() {
        assert!(is_test_file("test_1.h2w"));
        assert!(is_test_file("test_123.h2w"));
        assert!(!is_test_file("test_1.txt"), "后缀不匹配不算测试文件");
        assert!(!is_test_file("other.h2w"), "前缀不匹配不算测试文件");
        assert!(!is_test_file("bench_test.tmp"), "测速临时文件不属于校验测试文件");
        assert!(!is_test_file(""), "空名字不算");
    }

    #[test]
    fn has_enough_space_edges() {
        // 恰好满足：space == file_size + margin
        assert!(has_enough_space(SPACE_MARGIN + 1024, 1024));
        // 差一个字节都不行
        assert!(!has_enough_space(SPACE_MARGIN + 1023, 1024));
        // 空间完全不足
        assert!(!has_enough_space(0, 1024));
        // 大文件边界
        assert!(has_enough_space(SPACE_MARGIN + 4096 * 1024 * 1024, 4096 * 1024 * 1024));
        assert!(!has_enough_space(SPACE_MARGIN + 4096 * 1024 * 1024 - 1, 4096 * 1024 * 1024));
    }

    #[test]
    fn log_queue_capped() {
        let mut app = AppState::default();
        for i in 0..300 {
            app.log(format!("msg {}", i));
        }
        assert_eq!(app.log_queue.len(), 200, "日志队列应被限制在 200 条");
        // 队首是最早的（第 100 条），队尾是最新的
        assert!(app.log_queue.front().unwrap().contains("msg 100"));
        assert!(app.log_queue.back().unwrap().contains("msg 299"));
    }

    #[test]
    fn poll_messages_flow() {
        let mut app = AppState::default();
        let (tx, rx) = mpsc::channel();
        app.task_rx = Some(rx);
        app.running = true;

        tx.send(TaskMsg::Log("hello".to_string())).unwrap();
        tx.send(TaskMsg::Progress(ProgressInfo {
            visible: true,
            phase: "写入阶段".to_string(),
            cur: 1,
            total: 2,
        }))
        .unwrap();
        app.poll_messages();
        assert_eq!(app.log_queue.len(), 1);
        assert!(app.log_queue.back().unwrap().contains("hello"));
        assert!(app.progress.visible);
        assert!(app.running, "未收到 Finished 前仍处于运行中");

        tx.send(TaskMsg::Finished).unwrap();
        app.poll_messages();
        assert!(!app.running, "收到 Finished 后应停止运行");
        assert!(app.task_rx.is_none(), "Finished 后应清空消息接收端");
        assert!(app.task_tx.is_none());
        assert!(!app.progress.visible, "Finished 后进度条应隐藏");
    }

    #[test]
    fn write_random_file_errors_on_dir() {
        let dir = temp_dir("wr_err");
        // 目标路径是目录，File::create 必然失败
        let r = write_random_file(&dir, 100, || false);
        assert!(r.is_err(), "向目录路径写入应返回错误而不是 panic");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hash_file_stops() {
        let dir = temp_dir("hash_stop");
        let path = dir.join("t.bin");
        write_random_file(&path, 10 * 1024 * 1024, || false).unwrap();
        let calls = AtomicUsize::new(0);
        let should_stop = || calls.fetch_add(1, Ordering::Relaxed) >= 4;
        let r = hash_file(&path, &should_stop).unwrap();
        assert!(r.is_none(), "校验过程中被停止应返回 Ok(None)");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn find_cjk_font_finds_system_font() {
        let p = find_cjk_font();
        assert!(
            p.is_some(),
            "系统应能发现一个中文字体（本机需安装 Noto CJK / 文泉驿等）"
        );
    }

    #[test]
    fn cjk_font_contains_chinese_glyphs() {
        let bytes = find_cjk_font()
            .and_then(|p| std::fs::read(p).ok())
            .expect("应能读取到中文字体文件");
        let font = ab_glyph::FontArc::try_from_vec(bytes).expect("字体文件应可被解析");
        for ch in ['中', '文', '校', '验', '盘', '符', '测', '速', 'U', '盘'] {
            assert_ne!(
                font.glyph_id(ch),
                ab_glyph::GlyphId(0),
                "字体应包含字形 {}",
                ch
            );
        }
    }
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
