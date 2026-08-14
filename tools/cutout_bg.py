# -*- coding: utf-8 -*-
"""抠图：去掉 assets/original-whale-girl.jpg 的白色背景
-> assets/whale-girl-cutout.png（透明背景）。

方法：
1) 从四边角落做洪水填充，定位与背景连通的区域（容忍 JPEG 噪点）；
2) 背景掩膜向外膨胀 2px 得到边缘环带，环带内按“到背景色的颜色距离”做软过渡 alpha；
3) 环带内做反混合（un-premultiply），去掉残留的白色描边。

路径按脚本所在目录解析，可在任意位置运行：python tools/cutout_bg.py
"""
import os
import numpy as np
from PIL import Image, ImageDraw, ImageFilter

BASE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(BASE, ".."))
SRC = os.path.join(ROOT, "assets", "original-whale-girl.jpg")
OUT = os.path.join(ROOT, "assets", "whale-girl-cutout.png")
FILL_THRESH = 30     # 洪水填充容差（背景连通区判定，JPEG 噪点容忍）
RING = 2             # 边缘环带半径（px）
D_IN, D_OUT = 18.0, 55.0   # 颜色距离 -> alpha 软过渡区间

im = Image.open(SRC).convert("RGB")
w, h = im.size
arr = np.array(im).astype(np.float32)
border = np.concatenate([arr[0, :, :], arr[-1, :, :], arr[:, 0, :], arr[:, -1, :]])
bg = np.median(border, axis=0)  # 背景色

# 1) 洪水填充：从四角把背景连通区标成洋红色
fl = im.copy()
magenta = (255, 0, 255)
for seed in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)]:
    ImageDraw.floodfill(fl, seed, magenta, thresh=FILL_THRESH)
is_bg = (np.abs(np.array(fl).astype(np.int16) - np.array(magenta)).sum(axis=2) < 10)

# 2) 到背景色的颜色距离
dist = np.sqrt(((arr - bg) ** 2).sum(axis=2))

# 3) 边缘环带 = 背景掩膜膨胀 RING px 后与自身的差集
bg_img = Image.fromarray((is_bg * 255).astype(np.uint8))
dil = np.array(bg_img.filter(ImageFilter.MaxFilter(RING * 2 + 1))) > 0
ring = dil & ~is_bg

# 4) alpha：背景 0，主体 255，环带按颜色距离软过渡
alpha = np.full((h, w), 255, np.float32)
alpha[is_bg] = 0.0
if ring.any():
    a = np.clip((dist[ring] - D_IN) / (D_OUT - D_IN), 0.0, 1.0) * 255.0
    alpha[ring] = a
    # 反混合：去掉环带内残留的白边（按 alpha 还原前景本色）
    aa = np.maximum(a / 255.0, 1e-3)
    fg = (arr[ring] - (1.0 - aa)[:, None] * bg[None, :]) / aa[:, None]
    arr[ring] = np.clip(fg, 0, 255)

out = np.dstack([arr, alpha]).astype(np.uint8)
Image.fromarray(out, "RGBA").save(OUT)

# 统计
print("size:", (w, h), "bg color:", bg.astype(int))
print("transparent fraction: %.3f" % (alpha < 1).mean())
print("ring pixels: %d, mean ring alpha: %.1f" % (ring.sum(), alpha[ring].mean() if ring.any() else 0))
print("center alpha:", alpha[h // 2, w // 2])
print("corner alphas:", [int(alpha[0, 0]), int(alpha[0, -1]), int(alpha[-1, 0]), int(alpha[-1, -1])])
print("OUT OK:", OUT)
