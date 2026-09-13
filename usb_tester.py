import tkinter as tk
from tkinter import ttk, messagebox, scrolledtext
import os
import secrets
import time
import string
import ctypes
import random
import shutil

class USBTesterGUI:
    def __init__(self, root):
        self.root = root
        self.root.title("开源U盘校验测速工具（仿H2testw）")
        self.root.geometry("820x620")
        self.selected_drive = tk.StringVar()
        self.file_size_mb = tk.IntVar(value=1024)
        self.running = False

        notebook = ttk.Notebook(root)
        notebook.pack(fill=tk.BOTH, expand=True, padx=10, pady=5)

        # Tab1：全盘写入校验（H2testw核心）
        tab_full = ttk.Frame(notebook)
        notebook.add(tab_full, text="全盘读写校验")
        self.build_full_test_tab(tab_full)

        # Tab2：简易测速（连续+4K随机）
        tab_bench = ttk.Frame(notebook)
        notebook.add(tab_bench, text="简易测速")
        self.build_bench_tab(tab_bench)

        # Tab3：USB信息（VID/PID）
        tab_info = ttk.Frame(notebook)
        notebook.add(tab_info, text="USB设备信息")
        self.build_usb_info_tab(tab_info)

        self.log_box = scrolledtext.ScrolledText(root, height=14)
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
        for c in string.ascii_uppercase:
            drv = f"{c}:\\"
            if os.path.exists(drv):
                if c.upper() != "C":
                    drives.append(drv)
        self.drive_combo["values"] = drives
        if drives:
            self.selected_drive.set(drives[0])
        self.log("盘符刷新完成，已排除C盘，请确认选中U盘！")

    def build_full_test_tab(self, parent):
        frame_top = ttk.Frame(parent, padding=10)
        frame_top.pack(fill=tk.X)
        ttk.Label(frame_top, text="目标盘符：").grid(row=0, column=0)
        self.drive_combo = ttk.Combobox(frame_top, textvariable=self.selected_drive, width=12)
        self.drive_combo.grid(row=0, column=1, padx=5)
        ttk.Button(frame_top, text="刷新盘符", command=self.refresh_drives).grid(row=0, column=2, padx=3)
        ttk.Label(frame_top, text="单文件大小(MB)：").grid(row=0, column=3, padx=(10,0))
        ttk.Spinbox(frame_top, from_=100, to=4096, textvariable=self.file_size_mb, width=8).grid(row=0, column=4)

        frame_btn = ttk.Frame(parent, padding=10)
        frame_btn.pack(fill=tk.X)
        self.btn_full_start = ttk.Button(frame_btn, text="全盘写入+校验", command=self.start_full_test)
        self.btn_full_start.pack(side=tk.LEFT, padx=5)
        self.btn_stop = ttk.Button(frame_btn, text="停止", command=self.stop_test, state=tk.DISABLED)
        self.btn_stop.pack(side=tk.LEFT, padx=5)
        ttk.Button(frame_btn, text="清理测试文件", command=self.clean_test_files).pack(side=tk.LEFT, padx=5)

    def build_bench_tab(self, parent):
        frame_btn = ttk.Frame(parent, padding=10)
        frame_btn.pack(fill=tk.X)
        ttk.Button(frame_btn, text="开始测速（连续+4K）", command=self.run_benchmark).pack(side=tk.LEFT, padx=5)
        ttk.Button(frame_btn, text="清理测速临时文件", command=self.clean_bench_file).pack(side=tk.LEFT, padx=5)

    def build_usb_info_tab(self, parent):
        frame_btn = ttk.Frame(parent, padding=10)
        frame_btn.pack(fill=tk.X)
        ttk.Button(frame_btn, text="读取USB VID/PID", command=self.read_usb_vid_pid).pack(side=tk.LEFT, padx=5)
        ttk.Label(parent, text="提示：仅获取VID/PID。拿到后去网上查ChipGenius数据库，才能查到主控型号，无法直接读取闪存信息。", foreground="#882200").pack(padx=10, pady=10)

    def clean_test_files(self):
        drv = self.selected_drive.get()
        cnt = 0
        try:
            for f in os.listdir(drv):
                if f.startswith("test_") and f.endswith(".h2w"):
                    fp = os.path.join(drv, f)
                    os.remove(fp)
                    cnt +=1
            self.log(f"清理完成，删除测试文件：{cnt}")
        except Exception as e:
            self.log(f"清理失败: {e}")

    def clean_bench_file(self):
        drv = self.selected_drive.get()
        path = os.path.join(drv, "bench_test.tmp")
        if os.path.exists(path):
            os.remove(path)
            self.log("测速临时文件已删除")

    def stop_test(self):
        self.running = False
        self.btn_full_start.config(state=tk.NORMAL)
        self.btn_stop.config(state=tk.DISABLED)
        self.log("测试已手动停止")

    def start_full_test(self):
        drv = self.selected_drive.get()
        if not drv:
            messagebox.showerror("错误", "请选择盘符")
            return
        if not messagebox.askyesno("警告", "全盘测试会写入大量数据，备份U盘数据后继续？"):
            return
        self.running = True
        self.btn_full_start.config(state=tk.DISABLED)
        self.btn_stop.config(state=tk.NORMAL)
        self.log("===== 全盘读写校验开始 =====")
        file_size = self.file_size_mb.get() * 1024 * 1024

        # ========== 修复：shutil.disk_usage 替代 os.statvfs ==========
        usage = shutil.disk_usage(drv)
        free = usage.free
        self.log(f"盘符剩余空间 {free/(1024**3):.2f} GB")

        file_idx = 1
        error_count = 0
        total_write = 0
        total_read = 0
        written = []
        t0 = time.time()
        while self.running:
            usage = shutil.disk_usage(drv)
            free = usage.free
            if free < file_size * 1.1:
                self.log(f"空间不足，停止写入。剩余 {free/(1024**3):.2f} GB")
                break
            fname = os.path.join(drv, f"test_{file_idx}.h2w")
            data = secrets.token_bytes(file_size)
            with open(fname, "wb") as f:
                f.write(data)
            written.append((fname, data))
            total_write += file_size
            self.log(f"已写入 test_{file_idx}.h2w")
            file_idx +=1
        t_write = time.time() - t0
        write_spd = (total_write / 1024 /1024) / t_write if t_write>0 else 0
        self.log(f"写入阶段完成，平均写入速度 {write_spd:.2f} MB/s")

        self.log("----- 开始回读校验 -----")
        t0r = time.time()
        for fname, orig in written:
            if not self.running: break
            try:
                with open(fname, "rb") as f:
                    rd = f.read()
                if rd == orig:
                    self.log(f"✅ {os.path.basename(fname)} OK")
                    total_read += len(rd)
                else:
                    self.log(f"❌ {os.path.basename(fname)} 校验失败，数据损坏")
                    error_count +=1
            except Exception as e:
                self.log(f"❌ 读取失败 {fname}: {e}")
                error_count +=1
        t_read = time.time() - t0r
        read_spd = (total_read /1024/1024)/t_read if t_read>0 else 0
        self.log("===== 全盘测试结果 =====")
        self.log(f"总文件 {len(written)}，错误文件 {error_count}")
        self.log(f"平均读取速度 {read_spd:.2f} MB/s")
        if error_count == 0:
            self.log("✅ 全部校验通过")
        else:
            self.log("❌ 发现数据损坏，扩容盘/闪存坏块")
        self.running = False
        self.btn_full_start.config(state=tk.NORMAL)
        self.btn_stop.config(state=tk.DISABLED)

    def run_benchmark(self):
        drv = self.selected_drive.get()
        tmp = os.path.join(drv, "bench_test.tmp")
        self.log("==== 简易测速开始 ====")
        # 连续读写 100MB
        size_seq = 100*1024*1024
        data_seq = secrets.token_bytes(size_seq)
        t0 = time.time()
        with open(tmp,"wb") as f: f.write(data_seq)
        t1 = time.time()
        write_seq = size_seq / 1024 /1024 / (t1-t0)
        t0 = time.time()
        with open(tmp,"rb") as f: _ = f.read()
        t1 = time.time()
        read_seq = size_seq /1024/1024/(t1-t0)
        self.log(f"连续写入: {write_seq:.2f} MB/s | 连续读取: {read_seq:.2f} MB/s")

        # 4K随机读写
        block = 4096
        count = 20000
        t0 = time.time()
        with open(tmp,"rb+") as f:
            for _ in range(count):
                pos = random.randint(0, size_seq-block)
                f.seek(pos)
                f.write(secrets.token_bytes(block))
        t1 = time.time()
        write_4k = (count*block)/1024/1024/(t1-t0)
        t0 = time.time()
        with open(tmp,"rb") as f:
            for _ in range(count):
                pos = random.randint(0, size_seq-block)
                f.seek(pos)
                f.read(block)
        t1 = time.time()
        read_4k = (count*block)/1024/1024/(t1-t0)
        self.log(f"4K随机写入: {write_4k:.3f} MB/s | 4K随机读取: {read_4k:.3f} MB/s")
        os.remove(tmp)
        self.log("==== 测速完成 ====")

    def read_usb_vid_pid(self):
        import subprocess
        self.log("枚举USB设备VID/PID：")
        try:
            res = subprocess.check_output(["powershell",
                "Get-PnpDevice -PresentOnly | Where-Object {$_.InstanceId -match '^USB'} | Select-Object InstanceId, FriendlyName"],
                encoding="utf-8")
            self.log(res)
        except Exception as e:
            self.log(f"读取USB信息失败:{e}")
        self.log("提示：找到类似 USB\\VID_xxxx&PID_xxxx 字符串，去ChipGenius数据库查询主控型号")

if __name__ == "__main__":
    root = tk.Tk()
    app = USBTesterGUI(root)
    root.mainloop()