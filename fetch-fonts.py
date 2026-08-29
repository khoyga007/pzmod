# Tai woff2 (latin, latin-ext, vietnamese) ve ui/fonts/ + sinh ui/fonts.css.
# Chay lai khi doi bo font. App desktop khong duoc phu thuoc mang de hien chu.
import io, os, re, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
UI = HERE
FONTS = os.path.join(UI, "fonts")
os.makedirs(FONTS, exist_ok=True)

KEEP = {
    "U+0000-00FF": "latin",
    "U+0100-02BA": "latin-ext",
    "U+0102-0103": "vietnamese",
}

src = io.open(os.path.join(UI, "gf.css"), encoding="utf-8").read()
blocks = re.findall(r"@font-face\s*\{[^}]*\}", src)
out, kept = [], 0

for b in blocks:
    ur = re.search(r"unicode-range:\s*([^;]+);", b)
    if not ur:
        continue
    tag = next((v for k, v in KEEP.items() if ur.group(1).strip().startswith(k)), None)
    if not tag:
        continue
    fam = re.search(r"font-family:\s*'([^']+)'", b).group(1)
    wt = re.search(r"font-weight:\s*(\d+)", b).group(1)
    url = re.search(r"url\((https://[^)]+)\)", b).group(1)
    name = "%s-%s-%s.woff2" % (fam.replace(" ", ""), wt, tag)
    path = os.path.join(FONTS, name)
    if not os.path.exists(path):
        urllib.request.urlretrieve(url, path)
    out.append(b.replace(url, "fonts/" + name).replace("url(fonts/", "url(fonts/"))
    kept += 1

io.open(os.path.join(UI, "fonts.css"), "w", encoding="utf-8", newline="\n").write(
    "/* Sinh boi fetch-fonts.py — dung sua tay. */\n" + "\n".join(out) + "\n")
print("kept %d faces -> fonts.css" % kept)
