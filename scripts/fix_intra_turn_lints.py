import re
from pathlib import Path

lib_path = Path("src/lib.rs")
lib_source = lib_path.read_text()
old_module = "pub(crate) mod compaction;\n"
if lib_source.count(old_module) != 1:
    raise SystemExit("unexpected legacy compaction module declaration")
lib_path.write_text(lib_source.replace(old_module, ""))

provider_path = Path("src/provider.rs")
provider_source = provider_path.read_text()
provider_source, summarize_count = re.subn(
    r"\n    /// Summarize only the history selected for removal\..*?\n"
    r"    pub\(crate\) fn summarize\(.*?\n"
    r"    \}\n\n"
    r"(?=    pub\(crate\) fn summarize_prepared)",
    "\n",
    provider_source,
    count=1,
    flags=re.DOTALL,
)
if summarize_count != 1:
    raise SystemExit(f"unexpected legacy summarize method count: {summarize_count}")
provider_path.write_text(provider_source)

app_path = Path("src/app.rs")
app_source = app_path.read_text()
old_wrapper = "fn find_compaction_boundary(\n"
if app_source.count(old_wrapper) != 1:
    raise SystemExit("unexpected boundary wrapper")
app_path.write_text(app_source.replace(old_wrapper, "#[cfg(test)]\nfn find_compaction_boundary(\n"))
