# -*- coding: utf-8 -*-
"""把 layout_dump 导出的真实布局 JSON 画成示意图（严格按 egui 实测坐标）。
用法: python draw_layout.py <json> <png> [裁剪说明]"""
import json
import sys

from PIL import Image, ImageDraw, ImageFont

MSYH = r"C:\Windows\Fonts\msyh.ttc"


def font(size):
    return ImageFont.truetype(MSYH, int(round(size)))


def wrap_cjk(draw, s, f, max_w):
    """按像素宽度折行；中文逐字断，英文/数字整体搬。"""
    if max_w <= 1:
        return [s]
    lines = []
    cur = ""
    i = 0
    n = len(s)
    while i < n:
        ch = s[i]
        if ch == "\n":
            lines.append(cur)
            cur = ""
            i += 1
            continue
        # 英文单词整体处理
        if ch.isascii() and (ch.isalnum() or ch in "/._-:?&=+#"):
            j = i
            while j < n and s[j].isascii() and (s[j].isalnum() or s[j] in "/._-:?&=+#"):
                j += 1
            token = s[i:j]
        else:
            token = ch
            j = i + 1
        trial = cur + token
        if draw.textlength(trial, font=f) <= max_w or not cur:
            cur = trial
            i = j
        else:
            lines.append(cur)
            cur = ""
    if cur:
        lines.append(cur)
    return lines or [""]


def main():
    src, dst = sys.argv[1], sys.argv[2]
    crop = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0
    with open(src, "r", encoding="utf-8") as f:
        data = json.load(f)

    W = int(data["win_w"])
    H = int(data["win_h"])
    if crop:
        H = int(crop)
    scale = 1.0

    img = Image.new("RGB", (int(W * scale), int(H * scale)), (245, 246, 248))
    d = ImageDraw.Draw(img, "RGBA")

    # 先画底：页面底色铺满
    d.rectangle([0, 0, W, H], fill=(245, 246, 248, 255))

    # 矩形：按面积从大到小排序，保证小控件画在大卡片之上
    rects = sorted(data["rects"], key=lambda r: -(r["w"] * r["h"]))
    for r in rects:
        x, y, w, h = r["x"], r["y"], r["w"], r["h"]
        if y > H or y + h < 0:
            continue
        f = r["fill"]
        if f[3] == 0:
            continue
        cr = min(r["cr"], int(min(w, h) / 2))
        d.rounded_rectangle(
            [x, y, x + w, y + h], radius=cr, fill=(f[0], f[1], f[2], f[3])
        )
        s = r["stroke"]
        if s[3] > 0 and r["sw"] > 0:
            d.rounded_rectangle(
                [x, y, x + w, y + h],
                radius=cr,
                outline=(s[0], s[1], s[2], s[3]),
                width=max(1, int(r["sw"])),
            )

    # 文字
    for t in data["texts"]:
        s = t["s"]
        if not s.strip():
            continue
        y = t["y"]
        if y > H:
            continue
        c = t["color"]
        rows = t.get("rows") or []
        row_h = rows[0]["h"] if rows else t["h"]
        size = row_h / 1.36
        f = font(size)
        if rows:
            # egui 自己折的行，直接照搬
            for r in rows:
                d.text(
                    (t["x"] + r["x"], y + r["dy"]),
                    r["s"],
                    font=f,
                    fill=(c[0], c[1], c[2], 255),
                )
        else:
            for i, line in enumerate(wrap_cjk(d, s, f, 440)):
                d.text(
                    (t["x"], y + i * t["h"]),
                    line,
                    font=f,
                    fill=(c[0], c[1], c[2], 255),
                )

    img.save(dst)
    print(f"{dst}  {img.size[0]}x{img.size[1]}")


if __name__ == "__main__":
    main()