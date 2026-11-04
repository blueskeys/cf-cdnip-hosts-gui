# -*- coding: utf-8 -*-
"""再生成 UI 内嵌字体子集(assets/cjk-subset.ttf + assets/symbols-subset.ttf)。

字符集 = src/ 全部源码字符(UI 文案 + 注释)+ 可打印 ASCII + 常用中英标点缓冲。
CJK 取自系统「微软雅黑 UI」(msyh.ttc),雅黑缺字再落到「Segoe UI Symbol」。

用法:python tools/make_font_subset.py   (在项目根目录执行,仅 Windows)
新增 UI 文案出现"方框/乱码"时,重跑本脚本后重新编译即可。
"""
import subprocess
import sys
from pathlib import Path

from fontTools.subset import main as subset_main
from fontTools.ttLib import TTFont

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
CJK_SRC = Path("C:/Windows/Fonts/msyh.ttc")
SYM_SRC = Path("C:/Windows/Fonts/seguisym.ttf")
CJK_OUT = ROOT / "assets" / "cjk-subset.ttf"
SYM_OUT = ROOT / "assets" / "symbols-subset.ttf"

# 手动缓冲:源码没出现但运行期可能拼进 UI 的常用标点/符号
BUFFER = set("、。,·×÷±°℃%‰+*/\\|_~=<>[]{}()!?,.;:'\"`^$#@&?-–—―“”‘’…「」『』(,)〈〉《》【】!?→←↑↓⇒✓✗⚠⚡○●◎◇◆□■△▲▽▼")


def collect_chars() -> set:
    chars = {chr(c) for c in range(0x20, 0x7F)}  # 可打印 ASCII(数字/字母/标点)
    for f in SRC.rglob("*.rs"):
        chars |= set(f.read_text(encoding="utf-8"))
    chars |= BUFFER
    return chars


def ttc_font_number(path: Path, want: str) -> int:
    """在 TTC 里找 family 名含 want 的字体索引(微软雅黑 UI)。"""
    for i in range(8):
        try:
            t = TTFont(str(path), fontNumber=i, lazy=True)
        except Exception:
            break
        names = {t["name"].getDebugName(n) or "" for n in (1, 4, 16)}
        if any(want.lower() in (n or "").lower() for n in names):
            return i
    return 0


def run_subset(src, out, chars, font_number=None):
    text_file = ROOT / "target" / "subset-chars.txt"
    text_file.parent.mkdir(parents=True, exist_ok=True)
    text_file.write_text("".join(sorted(chars)), encoding="utf-8")
    argv = [
        str(src), f"--text-file={text_file}", f"--output-file={out}",
        "--no-hinting", "--no-layout-closure", "--layout-features=",
        "--name-IDs=1,2,4,6", "--notdef-outline",
    ]
    if font_number is not None:
        argv.append(f"--font-number={font_number}")
    sys.argv = ["pyftsubset"] + argv
    subset_main()


def cmap_of(path, font_number=None):
    kw = {"fontNumber": font_number} if font_number is not None else {}
    return set(TTFont(str(path), lazy=True, **kw).getBestCmap().keys())


def main():
    chars = {c for c in collect_chars() if not c.isspace()}
    cjk_num = ttc_font_number(CJK_SRC, "YaHei UI")
    print(f"字符集 {len(chars)} 个;msyh.ttc 微软雅黑 UI 索引 = {cjk_num}")

    cjk_cmap = cmap_of(CJK_SRC, cjk_num)
    missing = {c for c in chars if ord(c) not in cjk_cmap}
    run_subset(CJK_SRC, CJK_OUT, chars, font_number=cjk_num)

    # 雅黑缺字(✓✗⚠⚡ 等符号)落到 Segoe UI Symbol 子集
    sym_cmap = cmap_of(SYM_SRC)
    sym_chars = {c for c in missing if ord(c) in sym_cmap}
    if sym_chars:
        run_subset(SYM_SRC, SYM_OUT, sym_chars)
    else:
        sym_chars = set()

    size_kb = lambda p: p.stat().st_size // 1024
    print(f"cjk-subset.ttf    {size_kb(CJK_OUT):>4} KB  字形覆盖 {len(chars) - len(missing)}/{len(chars)}")
    print(f"symbols-subset.ttf {size_kb(SYM_OUT):>3} KB  兜底 {len(sym_chars)} 字:{''.join(sorted(sym_chars))}")

    dead = missing - sym_chars
    if dead:
        print(f"[警告] 两种系统字体都没有的字(将显示为方框):{''.join(sorted(dead))}")
        sys.exit(1)


if __name__ == "__main__":
    main()
