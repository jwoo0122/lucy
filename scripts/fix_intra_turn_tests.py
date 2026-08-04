from pathlib import Path

path = Path("src/intra_turn.rs")
source = path.read_text()
old = "        assert_eq!(plan.boundary, 3);"
new = "        assert_eq!(plan.boundary, 1);"
if source.count(old) != 1:
    raise SystemExit("unexpected intra-turn boundary assertion")
path.write_text(source.replace(old, new))
