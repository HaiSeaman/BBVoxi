# -*- coding: utf-8 -*-
"""把「现状」与「精简方案」两张布局图并排拼成一张对比图，并标注关键问题。
用法: python compare.py <now.png> <new.png> <out.png>"""
import sys

from PIL import Image, ImageDraw, ImageFont

MSYH = r"C:\Windows\Fonts\msyh.ttc"
GAP = 40
TOP = 96


def font(size, bold=False):
    return ImageFont.truetype(MSYH, size)


def main():
    now_p, new_p, out_p = sys.argv[1], sys.argv[2], sys.argv[3]
    a = Image.open(now_p).convert("RGB")
    b = Image.open(new_p).convert("RGB")

    H = max(a.height, b.height) + TOP + 60
    W = a.width + b.width + GAP * 3
    img = Image.new("RGB", (W, H), (238, 240, 244))
    d = ImageDraw.Draw(img, "RGBA")

    f_title = font(30, True)
    f_note = font(19)
    f_tag = font(20, True)

    def tag(x, y, s, color):
        """带底的标签，压在图上也看得清"""
        w = d.textlength(s, font=f_tag)
        d.rectangle([x - 4, y - 3, x + w + 4, y + 26], fill=(255, 255, 255, 235))
        d.text((x, y), s, font=f_tag, fill=color)

    x1 = GAP
    x2 = GAP + a.width + GAP
    y0 = TOP

    d.text((x1, 30), "现在（内容总高 1274px）", font=f_title, fill=(180, 40, 40))
    d.text((x2, 30), "精简后（内容总高 666px）", font=f_title, fill=(20, 130, 70))

    img.paste(a, (x1, y0))
    img.paste(b, (x2, y0))
    d.rectangle([x1 - 2, y0 - 2, x1 + a.width + 1, y0 + a.height + 1],
                outline=(180, 40, 40), width=2)
    d.rectangle([x2 - 2, y0 - 2, x2 + b.width + 1, y0 + b.height + 1],
                outline=(20, 130, 70), width=2)

    # 标注：现状里需要滚动才能看到的位置（900 高窗口，可滚动区 822）
    vis = 822
    if a.height > vis:
        yy = y0 + vis
        d.line([x1 - 6, yy, x1 + a.width + 6, yy], fill=(220, 60, 60), width=3)
        tag(x1 + 8, yy - 40, "900 高窗口只能看到这里", (200, 40, 40))
        d.text((x1 + 8, yy + 10), "↓ 以下必须滚动才看得到", font=f_note, fill=(150, 60, 60))

    if b.height > vis:
        yy = y0 + vis
        d.line([x2 - 6, yy, x2 + b.width + 6, yy], fill=(220, 60, 60), width=3)

    d.text((x1, y0 + a.height + 16),
           "卡片 5 张 · 说明小字占 30% · 768 屏要滚 744px", font=f_note,
           fill=(90, 90, 100))
    d.text((x2, y0 + b.height + 16),
           "卡片 2 张 · 说明收进气泡 · 900 屏起完整可见", font=f_note,
           fill=(90, 90, 100))

    img.save(out_p)
    print(f"{out_p}  {img.size[0]}x{img.size[1]}")


if __name__ == "__main__":
    main()