# -*- coding: utf-8 -*-
"""从源图片生成应用图标：
assets/whale-girl-cutout.png（鲸鱼少女抠图，透明背景，910x941）
-> assets/launcher.ico（多尺寸，用于 exe 与窗口标题栏）+ assets/launcher.png（GUI logo）。
Tauri 版用法：生成后把 assets/launcher.ico 复制为 tauri/src-tauri/icons/icon.ico，
把 assets/launcher.png 复制为 tauri/ui/logo.png，再 cargo build --release。
路径按脚本所在目录解析，可在任意位置运行：python tools/make_icon.py
"""
import os
from PIL import Image

BASE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(BASE, ".."))
SRC = os.path.join(ROOT, "assets", "whale-girl-cutout.png")
OUT_ICO = os.path.join(ROOT, "assets", "launcher.ico")
OUT_PNG = os.path.join(ROOT, "assets", "launcher.png")
PAD_RATIO = 0.10  # 图形四周留白比例


def make_square(src, size):
    im = Image.open(src).convert("RGBA")
    bbox = im.getbbox()
    if bbox:
        im = im.crop(bbox)
    # 等比缩放到 (1-2*pad) * size，居中粘贴
    max_w = int(size * (1 - 2 * PAD_RATIO))
    w, h = im.size
    scale = min(max_w / w, max_w / h)
    im = im.resize((max(1, int(w * scale)), max(1, int(h * scale))), Image.LANCZOS)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.paste(im, ((size - im.size[0]) // 2, (size - im.size[1]) // 2), im)
    return canvas


# launcher.png：GUI header 用（64px 足够清晰，存 128 备用）
make_square(SRC, 128).save(OUT_PNG)

# launcher.ico：多尺寸
sizes = [16, 24, 32, 48, 64, 128, 256]
base = make_square(SRC, 256)
base.save(OUT_ICO, sizes=[(s, s) for s in sizes])
print("ICON OK", os.path.getsize(OUT_ICO), "bytes; launcher.png",
      os.path.getsize(OUT_PNG), "bytes")
