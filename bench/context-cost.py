#!/usr/bin/env python3
"""Замер стоимости контекста: файловый подход против брифа Brain.

Считает, сколько токенов уходит в контекст агента при каждом запуске сессии.
Токенизатор: cl100k_base (tiktoken), как эталон подсчёта.
Без tiktoken — консервативная оценка по UTF-8 байтам.
"""
import json
import subprocess
import sys
from pathlib import Path

try:
    import tiktoken
    ENC = tiktoken.get_encoding("cl100k_base")
    def count(text: str) -> int:
        return len(ENC.encode(text))
    METHOD = "cl100k_base"
except ImportError:
    def count(text: str) -> int:
        return len(text.encode("utf-8")) // 4
    METHOD = "utf8-bytes/4 (приблизительно)"


def context_files(root: Path) -> list[Path]:
    """Файлы, которые агенты грузят в контекст на старте."""
    names = ["CLAUDE.md", "AGENTS.md", ".cursorrules", "GEMINI.md"]
    found = []
    for name in names:
        for path in [root / name, root / ".claude" / name]:
            if path.is_file():
                found.append(path)
    return found


def main() -> int:
    roots = [Path(p) for p in sys.argv[1:]] or [Path.cwd()]
    brain = Path(__file__).resolve().parent.parent / "target/release/brain"

    rows = []
    for root in roots:
        files = context_files(root)
        if not files:
            continue
        file_tokens = 0
        detail = []
        for path in files:
            text = path.read_text(encoding="utf-8", errors="replace")
            tokens = count(text)
            file_tokens += tokens
            detail.append({"file": str(path.relative_to(root)), "tokens": tokens})

        brief_tokens = None
        if brain.is_file():
            result = subprocess.run(
                [str(brain), "--offline", "brief"],
                capture_output=True, text=True, cwd=root, timeout=60,
            )
            if result.returncode == 0:
                try:
                    brief_tokens = count(json.loads(result.stdout)["text"])
                except (json.JSONDecodeError, KeyError):
                    pass

        rows.append({
            "project": root.name,
            "context_files": detail,
            "file_approach_tokens": file_tokens,
            "brain_brief_tokens": brief_tokens,
            "ratio": round(file_tokens / brief_tokens, 1) if brief_tokens else None,
        })

    print(json.dumps({"tokenizer": METHOD, "projects": rows}, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
