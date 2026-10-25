#!/usr/bin/env bash
# Pre-publication check: is anything personal or private about to be
# published? Scans every TRACKED file (and the git history with gitleaks,
# when installed) for:
#   - secrets (gitleaks),
#   - real-looking email addresses (anything not on a placeholder domain),
#   - phone numbers, home-directory paths, Fly.io machine ids,
#   - every term in a local deny-list (one per line, case-insensitive;
#     `term | allow: glob,glob` exempts files). The deny-list path defaults
#     to local/publish-denylist.txt — it is gitignored, so the sensitive
#     words themselves never enter the repository.
# Exit status 1 when anything is found. Usage:
#   scripts/check-public.sh [denylist-file]
set -uo pipefail
cd "$(dirname "$0")/.."
DENY="${1:-local/publish-denylist.txt}"
status=0

if command -v gitleaks >/dev/null 2>&1; then
  if ! gitleaks git . --no-banner --redact >/dev/null 2>&1; then
    echo "gitleaks: possible secrets — run: gitleaks git . --no-banner --redact"
    status=1
  fi
else
  echo "(gitleaks not installed — secret scan skipped)"
fi

python3 - "$DENY" <<'PY' || status=1
import fnmatch, os, re, subprocess, sys
deny_path = sys.argv[1]
files = [f for f in subprocess.run(["git", "ls-files"], capture_output=True, text=True).stdout.split("\n") if f]
skip = ("Cargo.lock", "crates/hearthloom-daemon/assets/vendor/")
# Placeholder domains (RFC 2606 names, one-letter domains like x.com) and
# automated system senders are not people.
placeholder = re.compile(r"@(?:[a-z0-9-]+\.)*(example(\.(com|org|net))?|test|invalid|localhost|"
                         r"users\.noreply\.github\.com|anthropic\.com|"
                         r"(?:resource|group)\.calendar\.google\.com)$|@[a-z]\.[a-z]{2,3}$", re.I)
system_sender = re.compile(r"^(no-?reply|notifications?|orders?|order-status|reservations?|"
                           r"support|billing|mailer-daemon|postmaster|reply\+[^@]*)@", re.I)
rules = [
    ("email", re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}\b")),
    ("phone", re.compile(r"(?<![\w.])(?:\+?1[ .-]?)?\(?[2-9]\d{2}\)?[ .-][2-9]\d{2}[ .-]\d{4}(?![\w])|\b1[2-9]\d{9}\.\d{11}\b")),
    ("home-path", re.compile(r"/(?:Users|home)/(?!you\b|me\b|user\b|username\b|<)[a-z][a-z0-9_-]+")),
    ("fly-machine-id", re.compile(r"(?<![0-9a-f])[0-9a-f]{14}(?![0-9a-f])")),
]
allowed = set()
if os.path.exists("scripts/check-public.allow"):
    allowed = {l.strip() for l in open("scripts/check-public.allow", encoding="utf-8")
               if l.strip() and not l.startswith("#")}
deny = []
if os.path.exists(deny_path):
    for raw in open(deny_path, encoding="utf-8"):
        raw = raw.strip()
        if not raw or raw.startswith("#"):
            continue
        term, _, allow = raw.partition("|")
        globs = [g.strip() for g in allow.replace("allow:", "").split(",") if g.strip()]
        deny.append((term.strip().lower(), globs))
else:
    print(f"(no deny-list at {deny_path} — only the generic checks ran)")
found = 0
for f in files:
    if f.startswith(skip) or not os.path.isfile(f):
        continue
    try:
        lines = open(f, encoding="utf-8").read().split("\n")
    except UnicodeDecodeError:
        continue
    for n, line in enumerate(lines, 1):
        hits = []
        for name, rx in rules:
            for m in rx.finditer(line):
                if name == "email" and (placeholder.search(m.group(0)) or system_sender.search(m.group(0))
                                        or m.group(0) in allowed):
                    continue
                if name == "phone" and re.search(r"555[ .-]?01\d\d|555[ .-]\d{4}", m.group(0)):
                    continue
                hits.append(f"{name}: {m.group(0)}")
        low = line.lower()
        for term, globs in deny:
            if term in low and not any(fnmatch.fnmatch(f, g) for g in globs):
                hits.append(f"deny-list: {term}")
        for h in hits:
            found += 1
            print(f"{f}:{n}: [{h}] {line.strip()[:110]}")
print(f"\n{found} finding(s) in {len(files)} tracked files.")
sys.exit(1 if found else 0)
PY
exit $status
