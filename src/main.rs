use inotify::{Inotify, WatchMask};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

#[macro_use]
extern crate log;

struct Directory<'a> {
    base: &'a Path,
    ext: Option<PathBuf>,
}

impl<'a> Directory<'a> {
    pub fn src(&self) -> PathBuf {
        let mut src = PathBuf::from(self.base);
        if let Some(ext) = &self.ext {
            src.push(ext);
        }
        src
    }

    pub fn dst(&self, root: &Path) -> PathBuf {
        let mut dst = PathBuf::from(root);
        dst.push(self.base.file_name().unwrap());
        if let Some(ext) = &self.ext {
            dst.push(ext);
        }
        dst
    }
}

struct Monitor<'a> {
    inotify: Inotify,
    dirs: Vec<(i32, Directory<'a>)>,
    dest: &'a Path,
    fail: bool,
}

impl<'a> Monitor<'a> {
    pub fn new(dest: &'a Path) -> Self {
        Self {
            inotify: Inotify::init().unwrap(),
            dirs: Vec::new(),
            dest,
            fail: false,
        }
    }

    fn add(&mut self, base: &'a Path, ext: Option<PathBuf>) {
        let dir = Directory { base, ext };
        let src = dir.src();
        let wd = self
            .inotify
            .watches()
            .add(&src, WatchMask::CREATE | WatchMask::MOVED_TO)
            .unwrap()
            .get_watch_descriptor_id();

        info!("Watching {}: {}", src.display(), wd);

        // `create_dir_all` succeeds if all directories already exist.
        fs::create_dir_all(dir.dst(self.dest)).unwrap();

        self.dirs.push((wd, dir));
    }

    pub fn watch(&mut self, dir: &'a Path) {
        self.add(dir, None);

        // Watch all subdirectories.
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if let Ok(typ) = entry.file_type() {
                if typ.is_dir() {
                    let ext = PathBuf::from(entry.file_name());
                    self.add(dir, Some(ext));
                }
            }
        }
    }

    pub fn finish(&mut self) {
        self.dirs.shrink_to_fit();
    }

    fn find_dir(&self, wd: i32) -> io::Result<&Directory> {
        for (dir_wd, dir) in &self.dirs {
            if wd == *dir_wd {
                return Ok(dir);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Unknown watch descriptor: {}", wd),
        ))
    }

    fn handle(&self, name: &Path, wd: i32) -> io::Result<()> {
        let dir = self.find_dir(wd)?;

        let mut dst = dir.dst(self.dest);
        dst.push(name);

        // Android's FUSE doesn't seem to support `RENAME_NOREPLACE` of
        // `renameat2`, so we have to avoid races using `O_EXCL`.
        drop(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dst)?,
        );

        let mut src = dir.src();
        src.push(name);

        fs::rename(&src, &dst)?;
        info!("Moved {} to {}", src.display(), dst.display());
        Ok(())
    }

    pub fn run(&mut self, buf: &mut [u8]) {
        match self.inotify.read_events_blocking(buf) {
            Ok(events) => {
                self.fail = false;
                let mut slept = false;
                for e in events {
                    debug!("{:?}: {:?}", e.name, e);
                    if let Some(name) = e.name {
                        if name.as_encoded_bytes()[0] == b'.' {
                            continue;
                        }
                        let name = Path::new(name);
                        if name
                            .extension()
                            .is_some_and(|ext| ext.to_ascii_lowercase() == "mp4")
                        {
                            // Wait for the gallery to finish media scanning, or
                            // an invalid entry will remain there.
                            if !slept {
                                std::thread::sleep(std::time::Duration::from_secs(1));
                                slept = true;
                            }
                            if let Err(e) = self.handle(name, e.wd.get_watch_descriptor_id()) {
                                error!("Error moving {}: {:?}", name.display(), e);
                            }
                        }
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

fn main() {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info");
    }
    pretty_env_logger::init_timed();

    // Consume `argv[0]` first.
    let mut args = env::args_os();
    args.next().unwrap();

    // First argument is the destination, and the rest are watched directories.
    let mut dest = PathBuf::from(args.next().unwrap());
    dest.shrink_to_fit();
    let mut dirs = args.collect::<Vec<_>>();
    dirs.shrink_to_fit();

    let mut m = Monitor::new(&dest);
    for dir in &mut dirs {
        dir.shrink_to_fit();
        m.watch(Path::new(dir));
    }
    m.finish();

    // `inotify` takes a mutable reference to the buffer, so we can't use
    // uninitialized memory.
    let mut buf = [0; 1024];
    loop {
        m.run(&mut buf);
    }
}
