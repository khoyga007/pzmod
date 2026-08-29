# Sinh pzmod-rs/ui/ tu goc repo. ui.html o goc la ban duy nhat — dung sua ui/index.html.
# Tauri nap frontendDist tu thu muc nay, nen tokens/fonts phai nam trong do.
import io, os, shutil

here = os.path.dirname(os.path.abspath(__file__))
root = os.path.dirname(here)
ui = os.path.join(here, "ui")

src = io.open(os.path.join(root, "ui.html"), encoding="utf-8").read()
assert src.count("<script>") == 1, "ui.html doi cau truc, xem lai sync-ui.py"
# Chen bridge.js truoc script chinh: no doi fetch("/api/...") thanh invoke().
out = src.replace("<script>", '<script src="bridge.js"></script>\n<script>', 1)
# Tauri phuc vu asset tu goc site nen duong dan tuyet doi van dung; giu nguyen.
io.open(os.path.join(ui, "index.html"), "w", encoding="utf-8", newline="\n").write(out)

for name in ("tokens.css", "fonts.css"):
    shutil.copyfile(os.path.join(root, name), os.path.join(ui, name))

fonts = os.path.join(ui, "fonts")
shutil.rmtree(fonts, ignore_errors=True)
shutil.copytree(os.path.join(root, "fonts"), fonts)

print("ui/ synced: index.html + tokens.css + fonts.css + %d font files"
      % len(os.listdir(fonts)))
