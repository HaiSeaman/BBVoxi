"""把带棋盘格背景的原始图标转成透明 PNG / ICO / 原始 RGBA 资源。

关键点：不能简单地「接近白色就算透明」——图案内部的高光也是白色的，那样会被挖空、
整体发灰。正确做法是从图像外边界做连通域填充，只有与外部连通的背景才透明。

用法：python scripts/make_icons.py <源图>
输出到 assets/：icon.png / icon.ico / rgba128_window.bin / rgba32_{idle,rec,err}.bin
"""

import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "assets"


def strip_background(img: Image.Image) -> Image.Image:
    rgb = np.asarray(img.convert("RGB"))
    mx = rgb.max(axis=2).astype(np.int16)
    mn = rgb.min(axis=2).astype(np.int16)
    sat = mx - mn

    # 棋盘格的两个色签名（实测：纯白 ≥242，浅灰 ~224），要求近乎无彩度，
    # 这样图案内部带色偏的高光（sat≥25）会被保留
    checker = (sat <= 10) & ((mn >= 242) | ((mn >= 214) & (mn <= 236)))

    # 外部连通背景：更宽的阈值 + 从边界洪水填充。
    # 阈值放宽是为了让填充能穿过 JPEG 压缩产生的中灰噪点「路障」，
    # 图案主体彩度远高于阈值，不会被误吞；图案内部的高光不与外部连通，也不会被吞。
    bgish = (sat < 70) & (mn > 170)
    # 注意：Image.fromarray 得到的是只读图，floodfill 在只读图上会「静默不生效」
    # （不抛异常也不填充），必须先转成可写图像。
    mask_img = Image.fromarray(np.where(bgish, 255, 0).astype(np.uint8), "L").convert("L")
    mask_img.readonly = False
    h, w = bgish.shape
    for seed in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1), (w // 2, 0), (0, h // 2)]:
        if mask_img.getpixel(seed) == 255:
            ImageDraw.floodfill(mask_img, seed, 128, thresh=0)
    outside = np.array(mask_img) == 128

    transparent = checker | outside

    # 去孤立噪点：8 邻域已全部透明的像素也判为透明（JPEG 噪点残留），跑两轮
    for _ in range(2):
        surrounded = np.ones_like(transparent)
        for dy in (-1, 0, 1):
            for dx in (-1, 0, 1):
                if dy == 0 and dx == 0:
                    continue
                surrounded &= np.roll(transparent, (dy, dx), axis=(0, 1))
        transparent = transparent | surrounded

    alpha = np.where(transparent, 0, 255).astype(np.uint8)

    # 边界 1 像素做羽化，消掉锯齿（只平滑紧贴透明区的那一圈）
    near = np.zeros_like(transparent)
    for dy, dx in ((1, 0), (-1, 0), (0, 1), (0, -1)):
        near |= np.roll(transparent, (dy, dx), axis=(0, 1))
    edge = near & ~transparent
    a = alpha.astype(np.float32)
    blurred = (
        a + np.roll(a, 1, 0) + np.roll(a, -1, 0) + np.roll(a, 1, 1) + np.roll(a, -1, 1)
    ) / 5.0
    a[edge] = blurred[edge]
    alpha = np.clip(a, 0, 255).astype(np.uint8)

    return Image.fromarray(np.dstack([rgb, alpha]), "RGBA")


def square(img: Image.Image, margin_ratio: float = 0.04) -> Image.Image:
    """裁到内容外框，再补成正方形（留一点边距）"""
    bbox = img.getbbox()
    if bbox:
        img = img.crop(bbox)
    side = int(max(img.size) * (1 + margin_ratio * 2))
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.paste(img, ((side - img.width) // 2, (side - img.height) // 2), img)
    return canvas


def with_badge(base: Image.Image, size: int, color: tuple) -> Image.Image:
    """右下角叠状态圆点（录音中/错误）。做小一点，别盖住主体图案。"""
    icon = base.resize((size, size), Image.LANCZOS).convert("RGBA")
    draw = ImageDraw.Draw(icon)
    r = max(3, int(size * 0.16))
    cx = cy = size - r - 2
    draw.ellipse([cx - r - 1, cy - r - 1, cx + r + 1, cy + r + 1], fill=(255, 255, 255, 245))
    draw.ellipse([cx - r, cy - r, cx + r, cy + r], fill=color)
    return icon


def dump_rgba(img: Image.Image, path: Path) -> None:
    path.write_bytes(img.convert("RGBA").tobytes())


def main() -> None:
    src = Path(sys.argv[1])
    OUT.mkdir(parents=True, exist_ok=True)
    art = square(strip_background(Image.open(src)))

    art.resize((512, 512), Image.LANCZOS).save(OUT / "icon.png")
    art.save(OUT / "icon.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
    dump_rgba(art.resize((128, 128), Image.LANCZOS), OUT / "rgba128_window.bin")
    dump_rgba(art.resize((32, 32), Image.LANCZOS), OUT / "rgba32_idle.bin")
    dump_rgba(with_badge(art, 32, (255, 77, 79, 255)), OUT / "rgba32_rec.bin")
    dump_rgba(with_badge(art, 32, (255, 159, 67, 255)), OUT / "rgba32_err.bin")

    opaque = np.asarray(art)[..., 3]
    print(
        f"已生成 {len(list(OUT.iterdir()))} 个文件；"
        f"不透明像素占比 {(opaque > 200).mean():.1%}，"
        f"完全透明占比 {(opaque == 0).mean():.1%}"
    )


if __name__ == "__main__":
    main()
