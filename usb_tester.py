import tkinter as tk
from tkinter import ttk, messagebox, scrolledtext, filedialog
import os
import secrets
import time
import string

class H2LikeTesterGUI:
    def __init__(self, root):
        self.root = root
        self.root.title("开源U盘读写校验工具（仿H2testw）")
        self.root.geometry("750x550")
        self.selected_drive = tk.StringVar()
        self.file_size_mb = tk.IntVar(value=1024) # 默认1GB每个测试文件
        self.running = False

        # 界面布局
        frame_top = ttk.Frame(root, padding=10)
        frame_top.pack(fill=tk.X)

        ttk.Label(frame_top, text="选择目标盘符：").grid(row=0, column=0, sticky="w")
        self.drive_combo = ttk.Combobox(frame_top, textvariable=self.selected_drive, width=12)
        self.drive_combo.grid(row=0, column=1, padx=5)
        ttk.Button(frame_top, text="刷新盘符", command=self.refresh_drives).grid(row=0, column=2, padx=3)

        ttk.Label(frame_top, text="单文件大小(MB)：").grid(row=0, column=3, padx=(10,0))
        ttk.Spinbox(frame_top, from_=100, to=4096, textvariable=self.file_size_mb, width=8).grid(row=0, column=4)

        frame_btn = ttk.Frame(root, padding=10)
        frame_btn.pack(fill=tk.X)
        self.btn_start = ttk.Button(frame_btn, text="开始写入+校验", command=self.start_test)
        self.btn_start.pack(side=tk.LEFT, padx=5)
        self.btn_stop = ttk.Button(frame_btn, text="停止", command=self.stop_test, state=tk.DISABLED)
        self.btn_stop.pack(side=tk.LEFT, padx=5)
        ttk.Button(frame_btn, text="清理测试文件", command=self.clean_test_files).pack(side=tk.LEFT, padx=5)
        ttk.Button(frame_btn, text="保存日志", command=self.save_log).pack(side=tk.RIGHT, padx=5)

        # 日志窗口
        ttk.Label(root, text="运行日志：", padding=(10,0)).pack(anchor="w")
        self.log_box = scrolledtext.ScrolledText(root, height=18)
        self.log_box.pack(padx=10, pady=5, fill=tk.BOTH, expand=True)

        self.refresh_drives()

    def log(self, msg):
        t = time.strftime("%H:%M:%S", time.localtime())
        line = f"[{t}] {msg}\n"
        self.log_box.insert(tk.END, line)
        self.log_box.see(tk.END)
        self.root.update_idletasks()

    def refresh_drives(self):
        drives = []
        # 扫描Windows盘符
        for c in string.ascii_uppercase:
            drv = f"{c}:\\"
            if os.path.exists(drv):
                # 简单过滤：跳过系统盘C盘
                if c.upper() != "C":
                    drives.append(drv)
        self.drive_combo["values"] = drives
        if drives:
            self.selected_drive.set(drives[0])
        self.log("盘符刷新完成，已自动排除C盘，请确认选中U盘！")

    def clean_test_files(self):
        drv = self.selected_drive.get()
        if not drv:
            messagebox.showwarning("提示", "先选择盘符")
            return
        cnt = 0
        try:
            for f in os.listdir(drv):
                if f.startswith("test_") and f.endswith(".h2w"):
                    fp = os.path.join(drv, f)
                    os.remove(fp)
                    cnt +=1
            self.log(f"清理完成，删除测试文件数量：{cnt}")
        except Exception as e:
            self.log(f"清理失败: {e}")

    def save_log(self):
        content = self.log_box.get("1.0", tk.END)
        fp = filedialog.asksaveasfilename(defaultextension=".txt", filetypes=[("Text","*.txt")])
        if fp:
            with open(fp, "w", encoding="utf-8") as f:
                f.write(content)
            self.log("日志已保存")

    def stop_test(self):
        self.running = False
        self.btn_start.config(state=tk.NORMAL)
        self.btn_stop.config(state=tk.DISABLED)
        self.log("用户手动停止测试")

    def start_test(self):
        drv = self.selected_drive.get()
        if not drv:
            messagebox.showerror("错误", "请选择U盘盘符！")
            return
        if not messagebox.askyesno("警告", "测试会写入大量文件，占用U盘空间！\n请确认已备份U盘数据，继续？"):
            return

        self.running = True
        self.btn_start.config(state=tk.DISABLED)
        self.btn_stop.config(state=tk.NORMAL)
        self.log("======= 开始U盘读写校验测试 =======")
        file_size = self.file_size_mb.get() * 1024 * 1024
        drive_free = os.statvfs(drv).f_frsize * os.statvfs(drv).f_bavail
        self.log(f"盘符 {drv} 剩余空间: {drive_free/(1024**3):.2f} GB")

        file_idx = 1
        error_count = 0
        total_write_bytes = 0
        total_read_bytes = 0
        write_start_time = time.time()

        # 写入阶段
        self.log("---------- 写入阶段 ----------")
        written_files = []
        while self.running:
            free = os.statvfs(drv).f_frsize * os.statvfs(drv).f_bavail
            if free < file_size * 1.1:
                self.log(f"剩余空间不足，停止写入。剩余空间 {free/(1024**3):.2f} GB")
                break
            fname = os.path.join(drv, f"test_{file_idx}.h2w")
            self.log(f"生成 {os.path.basename(fname)} 大小: {file_size/(1024*1024):.0f} MB")
            data = secrets.token_bytes(file_size)
            with open(fname, "wb") as f:
                f.write(data)
            written_files.append((fname, data))
            total_write_bytes += file_size
            file_idx +=1
        write_cost = time.time() - write_start_time
        write_speed_mbs = (total_write_bytes / (1024*1024)) / write_cost if write_cost>0 else 0
        self.log(f"写入完成。总写入 {total_write_bytes/(1024**3):.2f} GB，平均速度 {write_speed_mbs:.2f} MB/s")

        if not self.running:
            return

        # 校验阶段
        self.log("---------- 回读校验阶段 ----------")
        read_start_time = time.time()
        for fname, original_data in written_files:
            if not self.running:
                break
            self.log(f"校验 {os.path.basename(fname)}")
            try:
                with open(fname, "rb") as f:
                    read_data = f.read()
                if read_data == original_data:
                    self.log(f"✅ {os.path.basename(fname)} 校验通过")
                    total_read_bytes += len(read_data)
                else:
                    self.log(f"❌ {os.path.basename(fname)} 数据损坏！")
                    error_count +=1
            except Exception as e:
                self.log(f"❌ 读取失败 {fname}: {e}")
                error_count +=1
        read_cost = time.time() - read_start_time
        read_speed_mbs = (total_read_bytes/(1024*1024)) / read_cost if read_cost>0 else 0

        # 结果汇总
        self.log("========== 测试结果 ==========")
        self.log(f"总校验文件数: {len(written_files)}")
        self.log(f"错误文件数: {error_count}")
        self.log(f"总读取 {total_read_bytes/(1024**3):.2f} GB，读取平均速度 {read_speed_mbs:.2f} MB/s")
        if error_count == 0:
            self.log("✅ 全部数据校验成功！容量真实，当前读写无错误")
        else:
            self.log("❌ 检测到数据损坏！大概率是扩容盘 / 闪存存在坏块")
        self.log("==============================\n")
        self.running = False
        self.btn_start.config(state=tk.NORMAL)
        self.btn_stop.config(state=tk.DISABLED)

if __name__ == "__main__":
    root = tk.Tk()
    app = H2LikeTesterGUI(root)
    root.mainloop()