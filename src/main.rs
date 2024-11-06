use inotify::{Event, EventMask, Inotify, WatchMask};
use log::Level;
use std::cell::{Cell, UnsafeCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{c_int, OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf, MAIN_SEPARATOR_STR};
use std::{env, fs, io};

#[macro_use]
extern crate log;

const CWD: &str = "/sdcard/DCIM";

const IMG_EXT_STRS: [&str; 10] = [
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "jxl", "apng",
];

struct Twice<T>(UnsafeCell<T>);

unsafe impl<T: Sync> Sync for Twice<T> {}

impl<T> Twice<T> {
    #[inline(always)]
    fn get(&self) -> *mut T {
        self.0.get()
    }
}

static IMG_EXTS: Twice<BTreeSet<&OsStr>> = Twice(UnsafeCell::new(BTreeSet::new()));

fn is_img_dir(dir: &Path) -> bool {
    dir.components().next().is_some_and(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.ends_with("Camera") || s.ends_with("_PRO") || s == "100ANDRO")
    })
}

fn is_ignored_dir(name: &OsStr, inplace: bool) -> bool {
    inplace && name.to_str().is_some_and(|s| s.starts_with("DSC_20"))
}

#[derive(Debug)]
struct Directory {
    path: PathBuf,
    ino: u64,
    allow_img: bool,
    dst_created: Cell<bool>,
}

impl Directory {
    pub fn new(mut path: PathBuf, ino: u64) -> Self {
        if !path.is_relative() {
            panic!("Path not relative: {}", path.display());
        }
        path.shrink_to_fit();
        let allow_img = is_img_dir(&path);
        Self {
            path,
            ino,
            allow_img,
            dst_created: Cell::new(false),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dst(&self, root: &Path) -> PathBuf {
        let r = root.join(&self.path);
        if !self.dst_created.replace(true) {
            fs::create_dir_all(&r).unwrap();
        }
        r
    }

    fn _join(&self, name: &Path) -> PathBuf {
        // We join only file names.
        let mut p = self.path.clone();
        let s = p.as_mut_os_string();
        s.push(MAIN_SEPARATOR_STR);
        s.push(name);
        p
    }

    pub fn join(&self, name: &Path) -> PathBuf {
        // Omit leading `./` in the paths.
        if self.path == Path::new(".") {
            name.to_path_buf()
        } else {
            self._join(name)
        }
    }

    pub fn join_owned(&self, name: OsString) -> PathBuf {
        if self.path == Path::new(".") {
            PathBuf::from(name)
        } else {
            self._join(name.as_ref())
        }
    }

    pub fn filter_name(&self, name: &Path) -> bool {
        return name.extension().is_some_and(|ext| {
            let ext = ext.to_ascii_lowercase();
            if ext == "mp4" || ext == "dng" {
                return true;
            }
            if !self.allow_img {
                unsafe {
                    return (*IMG_EXTS.get()).contains(ext.as_os_str());
                }
            }
            false
        });
    }
}

type Dirs = BTreeMap<c_int, Directory>;

#[derive(Debug)]
struct Monitor {
    inotify: Inotify,
    dirs: Dirs,
    inos: BTreeSet<u64>,
    dest: PathBuf,
    dry: bool,
    fail: bool,
}

impl Monitor {
    pub fn new(mut dest: PathBuf, dry: bool) -> Self {
        dest.shrink_to_fit();
        Self {
            dest,
            dry,
            inotify: Inotify::init().unwrap(),
            dirs: BTreeMap::new(),
            inos: BTreeSet::new(),
            fail: false,
        }
    }

    fn add(&mut self, dir: PathBuf) -> io::Result<c_int> {
        let st = dir.metadata()?;
        if !st.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Not a directory: {}", dir.display()),
            ));
        }

        let ino = st.ino();
        if self.inos.contains(&ino) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Duplicate ino: {} from {}", ino, dir.display()),
            ));
        }

        let wd = self
            .inotify
            .watches()
            .add(
                &dir,
                WatchMask::CREATE
                    | WatchMask::MOVED_TO
                    | WatchMask::DELETE_SELF
                    | WatchMask::MOVE_SELF,
            )
            .unwrap()
            .get_watch_descriptor_id();

        let dir = Directory::new(dir, ino);
        debug!("New: {:?}", dir);
        if dir.allow_img {
            info!("Watching {}: {} (Photos)", dir.path().display(), wd);
        } else {
            info!("Watching {}: {}", dir.path().display(), wd);
        }

        if let Some(old) = self.dirs.insert(wd, dir) {
            panic!("Duplicate wd: {} from {}", wd, old.path().display());
        }

        self.inos.insert(ino);
        Ok(wd)
    }

    fn remove(&mut self, wd: c_int) {
        if let Some(dir) = self.dirs.remove(&wd) {
            if !self.inos.remove(&dir.ino) {
                error!(
                    "Removing unknown inode: {} from {}, {}",
                    dir.ino,
                    dir.path().display(),
                    wd,
                );
            }
            info!("Unwatched {}: {}", dir.path().display(), wd);
        } else {
            error!("Removing unknown wd: {}", wd);
        }
    }

    pub fn watch(&mut self, root: PathBuf, inplace: bool) {
        // Watch all subdirectories recursively.
        match self.add(root) {
            Ok(wd) => {
                let dir = self.dirs.get(&wd).unwrap();
                let mut sub_dirs = vec![];
                for entry in fs::read_dir(dir.path()).unwrap().flatten() {
                    let name = entry.file_name();
                    if name.as_encoded_bytes()[0] == b'.' {
                        continue;
                    }

                    if let Ok(typ) = entry.file_type() {
                        if typ.is_dir() {
                            if is_ignored_dir(&name, inplace) {
                                debug!("Ignored: {}", name.to_string_lossy());
                            } else {
                                sub_dirs.push(dir.join_owned(name));
                            }
                        } else if inplace {
                            let name = Path::new(name.as_os_str());
                            if dir.filter_name(name) {
                                if let Err(e) = self.emit(dir, name, Level::Warn) {
                                    error!("Error moving {}: {:?}", dir.join(name).display(), e);
                                }
                            }
                        }
                    }
                }
                for sub_dir in sub_dirs {
                    self.watch(sub_dir, inplace);
                }
            }
            Err(e) => error!("Error watching: {:?}", e),
        }
    }

    fn find_dir(dirs: &Dirs, wd: c_int) -> io::Result<&Directory> {
        if let Some(dir) = dirs.get(&wd) {
            return Ok(dir);
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Unknown watch descriptor: {}", wd),
        ))
    }

    fn emit(&self, dir: &Directory, name: &Path, lvl: Level) -> io::Result<()> {
        let mut dst = dir.dst(&self.dest);
        let src = dir.join(name);
        dst.push(name);

        if self.dry {
            log!(lvl, "Dry run: {} -> {}", src.display(), dst.display());
            return Ok(());
        }

        // Android's FUSE doesn't seem to support `RENAME_NOREPLACE` of
        // `renameat2`, so we have to avoid races using `O_EXCL`.
        drop(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dst)?,
        );

        fs::rename(&src, &dst)?;
        log!(lvl, "Moved {}", src.display());
        Ok(())
    }

    fn handle_rm(&mut self, e: &Event<&OsStr>) -> io::Result<bool> {
        if e.mask.contains(EventMask::IGNORED) {
            // Handle it in another `DELETE_SELF` or `MOVE_SELF` event.
            return Ok(true);
        }

        if e.mask.contains(EventMask::DELETE_SELF) {
            // Here `e.name` is `None` and `EventMask::ISDIR` is unset.
            self.remove(e.wd.get_watch_descriptor_id());
            return Ok(true);
        }

        if e.mask.contains(EventMask::MOVE_SELF) {
            self.remove(e.wd.get_watch_descriptor_id());
            self.inotify.watches().remove(e.wd.clone())?;
            return Ok(true);
        }

        Ok(false)
    }

    fn handle(&mut self, e: &Event<&OsStr>, slept: &mut bool) -> io::Result<()> {
        if let Some(name) = e.name {
            if name.as_encoded_bytes()[0] == b'.' {
                return Ok(());
            }

            let name = Path::new(name);
            let dir = Self::find_dir(&self.dirs, e.wd.get_watch_descriptor_id())?;
            if e.mask.contains(EventMask::ISDIR) {
                self.watch(dir.join(name), false);
                return Ok(());
            }

            if dir.filter_name(name) {
                // Wait for the gallery to finish media scanning, or
                // an invalid entry will remain there.
                if !*slept {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    *slept = true;
                }

                if let Err(e) = self.emit(dir, name, Level::Info) {
                    error!("Error moving {}: {:?}", dir.join(name).display(), e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn run(&mut self, buf: &mut [u8]) {
        match self.inotify.read_events_blocking(buf) {
            Ok(events) => {
                self.fail = false;

                let mut evts = events.collect::<Vec<_>>();
                debug!("Got {} events: {:?}", evts.len(), evts);

                // Handle removals first to avoid adding duplicate inodes.
                evts.retain(|evt| match self.handle_rm(evt) {
                    Ok(true) => false,
                    Ok(false) => true,
                    Err(e) => {
                        error!("Error handling removal: {:?}: {:?}", evt, e);
                        false
                    }
                });

                let mut slept = false;
                for evt in evts {
                    if let Err(e) = self.handle(&evt, &mut slept) {
                        error!("Error handling: {:?}: {:?}", evt, e);
                    }
                }
            }
            Err(e) => {
                if self.fail {
                    // Don't retry too fast.
                    panic!("Error reading events again: {:?}", e);
                } else {
                    error!("Error reading events: {:?}", e);
                    self.fail = true;
                }
            }
        }
    }
}

struct Args {
    dest: PathBuf,
    dry: bool,
    inplace: bool,
}

impl Args {
    fn new() -> Self {
        // Consume `argv[0]` first.
        let mut args = env::args_os();
        args.next().unwrap();

        // First argument is the destination directory.
        let dest = PathBuf::from(args.next().unwrap());
        if !fs::metadata(&dest).unwrap().is_dir() {
            panic!("Not a directory: {}", dest.display());
        }

        let args = args.collect::<Vec<_>>();
        let dry = args.iter().any(|a| a == "-d");
        let inplace = args.iter().any(|a| a == "-i");
        let force = args.iter().any(|a| a == "-f");

        if !force {
            let cwd = env::current_dir().unwrap();
            if fs::metadata(&cwd).unwrap().ino() != fs::metadata(CWD).unwrap().ino() {
                panic!("Not in {}: {}", CWD, cwd.display());
            }
        }

        Self { dest, dry, inplace }
    }
}

fn main() {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info");
    }
    pretty_env_logger::init_timed();

    let args = Args::new();
    unsafe {
        for s in IMG_EXT_STRS {
            (*IMG_EXTS.get()).insert(OsStr::new(s));
        }
    }

    let mut m = Monitor::new(args.dest, args.dry);
    debug!("Monitor: {:?}", m);

    // Always start from the current directory.
    m.watch(PathBuf::from("."), args.inplace);

    let mut buf = [0; 1024];
    loop {
        m.run(&mut buf);
    }
}
