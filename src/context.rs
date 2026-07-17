use std::collections::BTreeMap;
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::{CStr, CString, OsString};
use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub struct ContextError(String);

impl ContextError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ContextError {}

impl From<io::Error> for ContextError {
    fn from(_error: io::Error) -> Self {
        Self::new("instruction context discovery error")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSource {
    pub path: PathBuf,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootContext {
    pub system_prompt: String,
    pub instruction_files: Vec<InstructionSource>,
    pub skills: Vec<SkillEntry>,
}

#[cfg(test)]
fn resolve_boot_context(
    home: &Path,
    cwd: &Path,
    configured_prompt: &str,
) -> Result<BootContext, ContextError> {
    resolve_boot_context_with_api_key_env(home, cwd, configured_prompt, None)
}

pub(crate) fn resolve_boot_context_with_api_key_env(
    home: &Path,
    cwd: &Path,
    configured_prompt: &str,
    api_key_env: Option<&str>,
) -> Result<BootContext, ContextError> {
    let cwd = fs::canonicalize(cwd)
        .map_err(|_error| ContextError::new("unable to resolve working directory"))?;
    let root = git_root(&cwd, api_key_env);
    let project_directories = ancestor_directories(&root, &cwd);

    let mut instruction_files = Vec::new();
    if let Some(instruction) = preferred_instruction(&home.join(".lucy"))? {
        instruction_files.push(instruction);
    }
    for directory in &project_directories {
        if let Some(instruction) = preferred_instruction(directory)? {
            instruction_files.push(instruction);
        }
    }

    let mut skills = BTreeMap::new();
    discover_skills(&home.join(".agents").join("skills"), &mut skills)?;
    for directory in &project_directories {
        discover_skills(&directory.join(".agents").join("skills"), &mut skills)?;
    }
    let skills = skills.into_values().collect::<Vec<_>>();
    let system_prompt = build_system_prompt(configured_prompt, &instruction_files, &skills);

    Ok(BootContext {
        system_prompt,
        instruction_files,
        skills,
    })
}

fn git_root(cwd: &Path, api_key_env: Option<&str>) -> PathBuf {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"]);
    if let Some(api_key_env) = api_key_env
        .map(str::trim)
        .filter(|api_key_env| !api_key_env.is_empty())
    {
        command.env_remove(api_key_env);
    }
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !text.is_empty() {
                if let Ok(path) = fs::canonicalize(text) {
                    return path;
                }
            }
            cwd.to_owned()
        }
        _ => cwd.to_owned(),
    }
}

fn ancestor_directories(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let mut current = cwd;
    loop {
        directories.push(current.to_owned());
        if current == root {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if !cwd.starts_with(parent) || !parent.starts_with(root) {
            break;
        }
        current = parent;
    }
    directories.reverse();
    directories
}

#[cfg(unix)]
struct ContextDirectory {
    file: fs::File,
}

#[cfg(not(unix))]
struct ContextDirectory {
    path: PathBuf,
}

#[cfg(unix)]
fn path_component_unavailable(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || error.raw_os_error() == Some(libc::ENOTDIR)
        || error.raw_os_error() == Some(libc::ELOOP)
}

#[cfg(unix)]
fn open_directory_at(parent: RawFd, name: &OsStr) -> io::Result<Option<fs::File>> {
    let name = CString::new(name.as_bytes())
        .map_err(|_error| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, 0) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if path_component_unavailable(&error) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(unsafe { fs::File::from_raw_fd(fd) }))
}

#[cfg(unix)]
fn open_file_at(parent: RawFd, name: &OsStr) -> io::Result<Option<fs::File>> {
    let name = CString::new(name.as_bytes())
        .map_err(|_error| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, 0) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if path_component_unavailable(&error) {
            return Ok(None);
        }
        return Err(error);
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    Ok(Some(file))
}

#[cfg(unix)]
impl ContextDirectory {
    fn open(path: &Path) -> io::Result<Option<Self>> {
        let start = if path.is_absolute() {
            OsStr::new("/")
        } else {
            OsStr::new(".")
        };
        let Some(file) = open_directory_at(libc::AT_FDCWD, start)? else {
            return Ok(None);
        };
        let mut directory = Self { file };

        for component in path.components() {
            let name = match component {
                Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "path prefix is not supported on Unix",
                    ));
                }
                Component::RootDir | Component::CurDir => continue,
                Component::ParentDir => OsStr::new(".."),
                Component::Normal(name) => name,
            };
            let Some(file) = open_directory_at(directory.file.as_raw_fd(), name)? else {
                return Ok(None);
            };
            directory = Self { file };
        }

        Ok(Some(directory))
    }

    fn open_child_directory(&self, name: &OsStr) -> io::Result<Option<Self>> {
        let Some(file) = open_directory_at(self.file.as_raw_fd(), name)? else {
            return Ok(None);
        };
        Ok(Some(Self { file }))
    }

    fn open_regular_file(&self, name: &OsStr) -> io::Result<Option<fs::File>> {
        open_file_at(self.file.as_raw_fd(), name)
    }

    fn entries(&self) -> io::Result<Vec<OsString>> {
        read_directory_entries(&self.file)
    }
}

#[cfg(not(unix))]
impl ContextDirectory {
    fn open(path: &Path) -> io::Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Ok(None),
            Ok(_) => Ok(Some(Self {
                path: path.to_owned(),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn open_child_directory(&self, name: &OsStr) -> io::Result<Option<Self>> {
        Self::open(&self.path.join(name))
    }

    fn open_regular_file(&self, name: &OsStr) -> io::Result<Option<fs::File>> {
        open_regular_file(&self.path.join(name))
    }

    fn entries(&self) -> io::Result<Vec<std::ffi::OsString>> {
        fs::read_dir(&self.path)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(unix)]
fn reset_directory_errno() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    unsafe {
        *libc::__error() = 0;
    }
}

#[cfg(unix)]
fn directory_errno() -> libc::c_int {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        unsafe { *libc::__errno_location() }
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        unsafe { *libc::__error() }
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        0
    }
}

#[cfg(unix)]
fn read_directory_entries(file: &fs::File) -> io::Result<Vec<OsString>> {
    let duplicate = unsafe { libc::dup(file.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    let directory = unsafe { libc::fdopendir(duplicate) };
    if directory.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(duplicate);
        }
        return Err(error);
    }
    let directory = DirectoryStream(directory);
    let mut entries = Vec::new();
    loop {
        reset_directory_errno();
        let entry = unsafe { libc::readdir(directory.0) };
        if entry.is_null() {
            let error_number = directory_errno();
            if error_number != 0 {
                return Err(io::Error::from_raw_os_error(error_number));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            entries.push(OsString::from_vec(name.to_vec()));
        }
    }
    Ok(entries)
}

#[cfg(not(unix))]
fn open_regular_file(path: &Path) -> io::Result<Option<fs::File>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Ok(None);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }

    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    Ok(Some(file))
}

fn read_open_file(mut file: fs::File) -> io::Result<String> {
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(contents)
}

fn preferred_instruction(directory: &Path) -> Result<Option<InstructionSource>, ContextError> {
    let Some(directory_fd) = ContextDirectory::open(directory)
        .map_err(|_error| ContextError::new("unable to inspect instruction context"))?
    else {
        return Ok(None);
    };

    for name in [OsStr::new("AGENTS.md"), OsStr::new("CLAUDE.md")] {
        let Some(file) = directory_fd
            .open_regular_file(name)
            .map_err(|_error| ContextError::new("unable to inspect instruction context"))?
        else {
            continue;
        };
        let contents = read_open_file(file)
            .map_err(|_error| ContextError::new("unable to read instruction context"))?;
        return Ok(Some(InstructionSource {
            path: directory.join(name),
            contents,
        }));
    }
    Ok(None)
}

fn discover_skills(
    skills_root: &Path,
    skills: &mut BTreeMap<String, SkillEntry>,
) -> Result<(), ContextError> {
    let Some(skills_parent_path) = skills_root.parent() else {
        return Ok(());
    };
    let Some(skills_parent) = ContextDirectory::open(skills_parent_path)
        .map_err(|_error| ContextError::new("unable to inspect skill context"))?
    else {
        return Ok(());
    };
    let Some(skills_name) = skills_root.file_name() else {
        return Ok(());
    };
    let Some(skills_directory) = skills_parent
        .open_child_directory(skills_name)
        .map_err(|_error| ContextError::new("unable to inspect skill context"))?
    else {
        return Ok(());
    };

    let entries = skills_directory
        .entries()
        .map_err(|_error| ContextError::new("unable to inspect skill context"))?;
    let mut directories = entries
        .into_iter()
        .map(|name| (skills_root.join(&name), name))
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| left.0.cmp(&right.0));

    for (path, name) in directories {
        let Some(directory) = skills_directory
            .open_child_directory(&name)
            .map_err(|_error| ContextError::new("unable to inspect skill context"))?
        else {
            continue;
        };
        let Some(file) = directory
            .open_regular_file(OsStr::new("SKILL.md"))
            .map_err(|_error| ContextError::new("unable to inspect skill context"))?
        else {
            continue;
        };
        let contents = match read_open_file(file) {
            Ok(contents) => contents,
            Err(_) => continue,
        };
        let Some((name, description)) = parse_skill_frontmatter(&contents) else {
            continue;
        };
        skills.insert(
            name.clone(),
            SkillEntry {
                name,
                description,
                path: path.join("SKILL.md"),
            },
        );
    }

    Ok(())
}

fn parse_skill_frontmatter(contents: &str) -> Option<(String, String)> {
    let lines = contents.lines().collect::<Vec<_>>();
    if lines.first().map(|line| line.trim()) != Some("---") {
        return None;
    }
    let end = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim() == "---")
        .map(|(index, _)| index)?;

    let mut name = None;
    let mut description = None;
    let mut index = 1;
    while index < end {
        let line = lines[index];
        let trimmed = line.trim_start();
        if let Some(value) = trimmed.strip_prefix("name:") {
            name = parse_scalar(value);
            index += 1;
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("description:") {
            let value = value.trim();
            if matches!(value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
                let folded = value.starts_with('>');
                index += 1;
                let mut block = Vec::new();
                while index < end {
                    let block_line = lines[index];
                    if !block_line.trim().is_empty() && !block_line.starts_with(char::is_whitespace)
                    {
                        break;
                    }
                    block.push(block_line.trim().to_owned());
                    index += 1;
                }
                description = Some(if folded {
                    block.join(" ").trim().to_owned()
                } else {
                    block.join("\n").trim().to_owned()
                });
                continue;
            }
            description = parse_scalar(value);
        }
        index += 1;
    }

    let name = name?.trim().to_owned();
    let description = description?.trim().to_owned();
    if name.is_empty() || description.is_empty() {
        return None;
    }
    Some((name, description))
}

fn parse_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        return serde_json::from_str(value).ok();
    }
    if value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2 {
        return Some(value[1..value.len() - 1].replace("''", "'"));
    }
    Some(value.to_owned())
}

fn build_system_prompt(
    configured_prompt: &str,
    instruction_files: &[InstructionSource],
    skills: &[SkillEntry],
) -> String {
    let mut sections = vec![configured_prompt.trim_end().to_owned()];
    for instruction in instruction_files {
        sections.push(format!(
            "## Instructions from {}\n{}",
            instruction.path.display(),
            instruction.contents.trim_end()
        ));
    }
    if !skills.is_empty() {
        let mut catalog = String::from("## Available skills\n");
        for skill in skills {
            catalog.push_str(&format!(
                "- name: {}\n  description: {}\n  path: {}\n",
                skill.name,
                skill.description,
                skill.path.display()
            ));
        }
        sections.push(catalog.trim_end().to_owned());
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temporary_tree() -> (PathBuf, PathBuf) {
        let home = loop {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "lucy-context-{stamp}-{}-{counter}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temp tree: {error}"),
            }
        };
        let home = fs::canonicalize(&home).expect("canonical temp tree");
        let project = home.join("project").join("nested");
        fs::create_dir_all(&project).expect("tree");
        Command::new("git")
            .arg("-C")
            .arg(home.join("project"))
            .args(["init", "-q"])
            .output()
            .expect("git init");
        (home, project)
    }

    #[test]
    fn context_uses_precedence_and_specific_skill_override() {
        let (home, cwd) = temporary_tree();
        let project = home.join("project");
        fs::create_dir_all(home.join(".lucy")).expect("global dir");
        fs::write(home.join(".lucy").join("CLAUDE.md"), "global claude").expect("global");
        fs::write(home.join(".lucy").join("AGENTS.md"), "global agents").expect("global agents");
        fs::write(project.join("CLAUDE.md"), "root claude").expect("root claude");
        fs::write(project.join("AGENTS.md"), "root agents").expect("root agents");
        fs::write(cwd.join("CLAUDE.md"), "nested claude").expect("nested claude");

        let global_skill = home.join(".agents/skills/shared/SKILL.md");
        let root_skill = project.join(".agents/skills/shared/SKILL.md");
        let nested_skill = cwd.join(".agents/skills/nested/SKILL.md");
        fs::create_dir_all(global_skill.parent().expect("parent")).expect("global skills");
        fs::create_dir_all(root_skill.parent().expect("parent")).expect("root skills");
        fs::create_dir_all(nested_skill.parent().expect("parent")).expect("nested skills");
        fs::write(
            global_skill,
            "---\nname: shared\ndescription: global description\n---\n# global",
        )
        .expect("global skill");
        fs::write(
            root_skill,
            "---\nname: shared\ndescription: root description\n---\n# root",
        )
        .expect("root skill");
        fs::write(
            &nested_skill,
            "---\nname: nested\ndescription: nested description\n---\n# nested",
        )
        .expect("nested skill");

        let context = resolve_boot_context(&home, &cwd, "configured").expect("context");
        assert_eq!(context.instruction_files.len(), 3);
        assert!(context.instruction_files[0]
            .contents
            .contains("global agents"));
        assert!(context.instruction_files[1]
            .contents
            .contains("root agents"));
        assert!(context.instruction_files[2]
            .contents
            .contains("nested claude"));
        assert!(!context.system_prompt.contains("root claude"));
        assert!(context.system_prompt.contains("root description"));
        assert!(!context.system_prompt.contains("global description"));
        assert!(context.system_prompt.contains("nested description"));
        assert!(context
            .system_prompt
            .contains(&nested_skill.display().to_string()));
        assert!(!context.system_prompt.contains("# nested"));

        fs::remove_dir_all(home).expect("remove tree");
    }

    #[test]
    fn context_failure_does_not_echo_a_secret_bearing_path() {
        let (home, _cwd) = temporary_tree();
        let missing = home.join("provider-secret-context-missing");
        let error = resolve_boot_context(&home, &missing, "configured")
            .expect_err("missing working directory");
        let message = error.to_string();
        assert!(message.contains("working directory"));
        assert!(!message.contains("provider-secret"));
        assert!(!message.contains(&missing.display().to_string()));
        fs::remove_dir_all(home).expect("remove tree");
    }

    #[cfg(unix)]
    #[test]
    fn context_ignores_symlinked_instruction_and_skill_sources() {
        let (home, cwd) = temporary_tree();
        let project = home.join("project");
        fs::create_dir_all(home.join(".lucy")).expect("global directory");
        let global_instruction_target = home.join("global-instructions.md");
        fs::write(&global_instruction_target, "symlinked global instructions")
            .expect("global target");
        symlink(&global_instruction_target, home.join(".lucy/AGENTS.md"))
            .expect("global instruction symlink");
        fs::write(home.join(".lucy/CLAUDE.md"), "real global instructions")
            .expect("global fallback");

        let project_instruction_target = home.join("project-instructions.md");
        fs::write(
            &project_instruction_target,
            "symlinked project instructions",
        )
        .expect("project target");
        symlink(&project_instruction_target, project.join("AGENTS.md"))
            .expect("project agents symlink");
        symlink(&project_instruction_target, project.join("CLAUDE.md"))
            .expect("project claude symlink");

        let global_skills = home.join(".agents/skills");
        fs::create_dir_all(&global_skills).expect("global skills");
        let linked_directory_target = home.join("linked-skill-directory");
        fs::create_dir(&linked_directory_target).expect("linked directory target");
        fs::write(
            linked_directory_target.join("SKILL.md"),
            "---\nname: linked-directory\ndescription: linked directory\n---\n",
        )
        .expect("linked directory skill");
        symlink(
            &linked_directory_target,
            global_skills.join("linked-directory"),
        )
        .expect("skill directory symlink");

        let linked_file_target = home.join("linked-skill-file.md");
        fs::write(
            &linked_file_target,
            "---\nname: linked-file\ndescription: linked file\n---\n",
        )
        .expect("linked file target");
        let linked_file_directory = global_skills.join("linked-file");
        fs::create_dir(&linked_file_directory).expect("linked file directory");
        symlink(&linked_file_target, linked_file_directory.join("SKILL.md"))
            .expect("skill file symlink");

        let valid_skill = global_skills.join("valid/SKILL.md");
        fs::create_dir_all(valid_skill.parent().expect("valid skill parent"))
            .expect("valid skill directory");
        fs::write(
            &valid_skill,
            "---\nname: valid\ndescription: valid skill\n---\n",
        )
        .expect("valid skill");

        let project_skill_target = home.join("project-skills");
        let project_skill = project_skill_target.join("root-only/SKILL.md");
        fs::create_dir_all(project_skill.parent().expect("project skill parent"))
            .expect("project skill target");
        fs::write(
            &project_skill,
            "---\nname: project-only\ndescription: project only\n---\n",
        )
        .expect("project skill");
        fs::create_dir_all(project.join(".agents")).expect("project agents directory");
        symlink(&project_skill_target, project.join(".agents/skills")).expect("skill root symlink");

        let context = resolve_boot_context(&home, &cwd, "configured").expect("context");
        assert_eq!(context.instruction_files.len(), 1);
        assert_eq!(
            context.instruction_files[0].contents,
            "real global instructions"
        );
        assert_eq!(context.skills.len(), 1);
        assert_eq!(context.skills[0].name, "valid");
        assert!(!context.system_prompt.contains("symlinked"));
        assert!(!context.system_prompt.contains("linked-directory"));
        assert!(!context.system_prompt.contains("linked-file"));
        assert!(!context.system_prompt.contains("project-only"));

        fs::remove_dir_all(home).expect("remove tree");
    }

    #[cfg(unix)]
    #[test]
    fn context_ignores_symlinked_intermediate_parents() {
        let (home, cwd) = temporary_tree();
        let linked_home_target = home.join("linked-home-target");
        fs::create_dir_all(linked_home_target.join(".lucy")).expect("linked Lucy directory");
        fs::write(
            linked_home_target.join(".lucy/AGENTS.md"),
            "symlinked intermediate instructions",
        )
        .expect("linked instructions");
        let linked_skill = linked_home_target.join(".agents/skills/linked/SKILL.md");
        fs::create_dir_all(linked_skill.parent().expect("linked skill parent"))
            .expect("linked skill directory");
        fs::write(
            &linked_skill,
            "---\nname: linked-intermediate\ndescription: linked intermediate\n---\n",
        )
        .expect("linked skill");
        let linked_home = home.join("linked-home");
        symlink(&linked_home_target, &linked_home).expect("linked home");

        let context = resolve_boot_context(&linked_home, &cwd, "configured").expect("context");
        assert!(context.instruction_files.is_empty());
        assert!(context.skills.is_empty());
        assert!(!context.system_prompt.contains("symlinked intermediate"));
        assert!(!context.system_prompt.contains("linked-intermediate"));

        fs::remove_dir_all(home).expect("remove tree");
    }

    #[test]
    fn invalid_skill_metadata_is_skipped() {
        let (home, cwd) = temporary_tree();
        let invalid = cwd.join(".agents/skills/invalid/SKILL.md");
        fs::create_dir_all(invalid.parent().expect("parent")).expect("skill dir");
        fs::write(invalid, "---\nname: invalid\n---\nbody").expect("skill");
        let context = resolve_boot_context(&home, &cwd, "configured").expect("context");
        assert!(context.skills.is_empty());
        assert!(!context.system_prompt.contains("invalid"));
        fs::remove_dir_all(home).expect("remove tree");
    }
}
