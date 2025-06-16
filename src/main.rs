use std::{
    env, fs,
    str::FromStr,
    collections::HashSet,
    ffi::{CString, OsStr},
    path::{Path, PathBuf},
    process::{Command, Stdio, exit},
    io::{Read, Result, Error, Write},
    fs::{File, write, read_to_string},
    os::unix::{fs::{MetadataExt, PermissionsExt}, process::CommandExt}
};

use which::which;
use walkdir::WalkDir;
use goblin::elf::Elf;
use flate2::read::DeflateDecoder;
use nix::{libc::execve, unistd::{AccessFlags, access}};
use include_file_compress::include_file_compress_deflate;


const SHARUN_NAME: &str = env!("CARGO_PKG_NAME");


fn get_interpreter(library_path: &str) -> Result<PathBuf> {
    let mut interpreters = Vec::new();
    if let Ok(ldname) = env::var("SHARUN_LDNAME") {
        if !ldname.is_empty() {
            interpreters.push(ldname)
        }
    } else {
        #[cfg(target_arch = "x86_64")]          // target x86_64-unknown-linux-musl
        interpreters.append(&mut vec![
            "ld-linux-x86-64.so.2".into(),
            "ld-musl-x86_64.so.1".into(),
            "ld-linux.so.2".into()
        ]);
        #[cfg(target_arch = "aarch64")]         // target aarch64-unknown-linux-musl
        interpreters.append(&mut vec![
            "ld-linux-aarch64.so.1".into(),
            "ld-musl-aarch64.so.1".into()
        ]);
    }
    for interpreter in interpreters {
        let interpreter_path = Path::new(library_path).join(interpreter);
        if interpreter_path.exists() {
            return Ok(interpreter_path)
        }
    }
    Err(Error::last_os_error())
}

fn realpath(path: &str) -> String {
    Path::new(path).canonicalize().unwrap().to_str().unwrap().to_string()
}

fn basename(path: &str) -> String {
    let pieces: Vec<&str> = path.rsplit('/').collect();
    pieces.first().unwrap().to_string()
}

fn dirname(path: &str) -> String {
    let mut pieces: Vec<&str> = path.split('/').collect();
    if pieces.len() == 1 || path.is_empty() {
        // return ".".to_string();
    } else if !path.starts_with('/') &&
        !path.starts_with('.') &&
        !path.starts_with('~') {
            pieces.insert(0, ".");
    } else if pieces.len() == 2 && path.starts_with('/') {
        pieces.insert(0, "");
    };
    pieces.pop();
    pieces.join(&'/'.to_string())
}

fn is_hardlink(path1: &Path, path2: &Path) -> bool {
    if let Ok(metadata1) = fs::metadata(path1) {
        if let Ok(metadata2) = fs::metadata(path2) {
            return metadata1.ino() == metadata2.ino()
        }
    }
    false
}

fn is_writable(path: &str) -> bool {
    access(path, AccessFlags::W_OK).is_ok()
}

fn is_file(path: &str) -> bool {
    Path::new(path).is_file()
}

fn is_exe(path: &PathBuf) -> bool {
    if let Ok(metadata) = fs::metadata(path) {
        return metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    false
}

fn is_elf32(path: &str) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut buff = [0u8; 5];
    file.read_exact(&mut buff)?;
    if &buff[0..4] != b"\x7fELF" {
        return Ok(false)
    }
    Ok(buff[4] == 1)
}

fn is_elf_section(file_path: &str, section_name: &str) -> Result<bool> {
    let mut file = File::open(file_path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    if let Ok(elf) = Elf::parse(&buffer) {
        if let Some(section_headers) = elf.section_headers.as_slice().get(..) {
            for section_header in section_headers {
                if let Some(name) = elf.shdr_strtab.get_at(section_header.sh_name) {
                    if name == section_name {
                        return Ok(true)
                    }
                }
            }
        }
    }
    Ok(false)
}

fn get_env_var<K: AsRef<OsStr>>(key: K) -> String {
    env::var(key).unwrap_or("".into())
}

fn add_to_env<K: AsRef<OsStr>, V: AsRef<OsStr>>(key: K, val: V) {
    let (key, val) = (key.as_ref(), val.as_ref().to_str().unwrap());
    let old_val = get_env_var(key);
    if old_val.is_empty() {
        env::set_var(key, val)
    } else if !old_val.contains(val) {
        env::set_var(key, format!("{val}:{old_val}"))
    }
}

fn read_dotenv(dotenv_dir: &str) -> Vec<String> {
    let mut unset_envs = Vec::new();
    let dotenv_path = PathBuf::from(format!("{dotenv_dir}/.env"));
    if dotenv_path.exists() {
        dotenv::from_path(&dotenv_path).ok();
        let data = read_to_string(&dotenv_path).unwrap_or_else(|err|{
            eprintln!("Failed to read .env file: {}: {err}", dotenv_path.display());
            exit(1)
        });
        for string in data.trim().split("\n") {
            let string = string.trim();
            if string.starts_with("unset ") {
                for var_name in string.split_whitespace().skip(1) {
                    unset_envs.push(var_name.into());
                }
            }
        }
    }
    unset_envs
}

fn gen_library_path(library_path: &str, lib_path_file: &String) {
    let mut new_paths: Vec<String> = Vec::new();
    WalkDir::new(library_path)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .for_each(|entry| {
            let name = entry.file_name().to_string_lossy();
            if name.ends_with(".so") || name.contains(".so.") {
                if let Some(parent) = entry.path().parent() {
                    if let Some(parent_str) = parent.to_str() {
                        if parent_str != library_path && parent.is_dir() &&
                            !new_paths.contains(&parent_str.into()) {
                            new_paths.push(parent_str.into());
                        }
                    }
                }
            }
        });
    if let Err(err) = write(lib_path_file,
        format!("+:{}", &new_paths.join(":"))
            .replace(":", "\n")
            .replace(library_path, "+")
    ) {
        eprintln!("Failed to write lib.path: {lib_path_file}: {err}");
        exit(1)
    } else {
        eprintln!("Write lib.path: {lib_path_file}")
    }
}

fn print_usage() {
    println!("[ {} ]

[ Usage ]: {SHARUN_NAME} [OPTIONS] [EXEC ARGS]...
    Use lib4bin for create 'bin' and 'shared' dirs

[ Arguments ]:
    [EXEC ARGS]...              Command line arguments for execution

[ Options ]:
     l,  lib4bin [ARGS]         Launch the built-in lib4bin
    -g,  --gen-lib-path         Generate a lib.path file
    -v,  --version              Print version
    -h,  --help                 Print help

[ Environments ]:
    SHARUN_WORKING_DIR=/path    Specifies the path to the working directory
    SHARUN_LDNAME=ld.so         Specifies the name of the interpreter
    SHARUN_DIR                  Sharun directory",
    env!("CARGO_PKG_DESCRIPTION"));
}

fn main() {
    let sharun: PathBuf = env::current_exe().unwrap();
    let mut exec_args: Vec<String> = env::args().collect();

    let mut sharun_dir = sharun.parent().unwrap().to_str().unwrap().to_string();
    let lower_dir = &format!("{sharun_dir}/../");
    if basename(&sharun_dir) == "bin" &&
       is_file(&format!("{lower_dir}{SHARUN_NAME}")) {
        sharun_dir = realpath(lower_dir)
    }

    env::set_var("SHARUN_DIR", &sharun_dir);

    let bin_dir = &format!("{sharun_dir}/bin");
    let shared_dir = &format!("{sharun_dir}/shared");
    let shared_bin = &format!("{shared_dir}/bin");
    let shared_lib = format!("{shared_dir}/lib");
    let shared_lib32 = format!("{shared_dir}/lib32");

    let arg0 = PathBuf::from(exec_args.remove(0));
    let arg0_name = arg0.file_name().unwrap();
    let arg0_dir = PathBuf::from(dirname(arg0.to_str().unwrap())).canonicalize()
        .unwrap_or_else(|_|{
            if let Ok(which_arg0) = which(arg0_name) {
                which_arg0.parent().unwrap().to_path_buf()
            } else {
                eprintln!("Failed to find ARG0 dir!");
                exit(1)
            }
    });
    let arg0_path = arg0_dir.join(arg0_name);

    let mut bin_name = if arg0_path.is_symlink() && arg0_path.canonicalize().unwrap() == sharun {
        arg0_name.to_str().unwrap().into()
    } else {
        basename(sharun.file_name().unwrap().to_str().unwrap())
    };

    if bin_name == SHARUN_NAME {
        if !exec_args.is_empty() {
            match exec_args[0].as_str() {
                "-v" | "--version" => {
                    println!("v{}", env!("CARGO_PKG_VERSION"));
                    return
                }
                "-h" | "--help" => {
                    print_usage();
                    return
                }
                "-g" | "--gen-lib-path" => {
                    for library_path in [shared_lib, shared_lib32] {
                        if Path::new(&library_path).exists() {
                            let lib_path_file = &format!("{library_path}/lib.path");
                            gen_library_path(&library_path, lib_path_file)
                        }
                    }
                    return
                }
                "l" | "lib4bin" => {
                    let lib4bin_compressed = include_file_compress_deflate!("lib4bin", 9);
                    let mut decoder = DeflateDecoder::new(&lib4bin_compressed[..]);
                    let mut lib4bin = Vec::new();
                    decoder.read_to_end(&mut lib4bin).unwrap();
                    drop(decoder);
                    exec_args.remove(0);
                    add_to_env("PATH", bin_dir);
                    let cmd = Command::new("bash")
                        .env("SHARUN", sharun)
                        .envs(env::vars())
                        .stdin(Stdio::piped())
                        .arg("-s").arg("--")
                        .args(exec_args)
                        .spawn();
                    match cmd {
                        Ok(mut bash) => {
                            bash.stdin.take().unwrap().write_all(&lib4bin).unwrap_or_else(|err|{
                                eprintln!("Failed to write lib4bin to bash stdin: {err}");
                                exit(1)
                            });
                            exit(bash.wait().unwrap().code().unwrap())
                        }
                        Err(err) => {
                            eprintln!("Failed to run bash: {err}");
                            exit(1)
                        }
                    }
                }
                _ => {
                    bin_name = exec_args.remove(0);
                    let bin_path = PathBuf::from(bin_dir).join(&bin_name);
                    if is_exe(&bin_path) &&
                        (is_hardlink(&sharun, &bin_path) ||
                        !Path::new(&shared_bin).join(&bin_name).exists())
                    {
                        add_to_env("PATH", bin_dir);
                        let err = Command::new(&bin_path)
                            .envs(env::vars())
                            .args(exec_args)
                            .exec();
                        eprintln!("Failed to run: {}: {err}", bin_path.display());
                        exit(1)
                    }
                }
            }
        } else {
            eprintln!("Specify the executable from: '{bin_dir}'");
            if let Ok(dir) = Path::new(bin_dir).read_dir() {
                for bin in dir.flatten() {
                    if is_exe(&bin.path()) {
                        println!("{}", bin.file_name().to_str().unwrap())
                    }
                }
            }
            exit(1)
        }
    } else if bin_name == "AppRun" {
        let appname_file = &format!("{sharun_dir}/.app");
        let mut appname: String = "".into();
        if !Path::new(appname_file).exists() {
            if let Ok(dir) = Path::new(&sharun_dir).read_dir() {
                for entry in dir.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        let name = entry.file_name();
                        let name = name.to_str().unwrap();
                        if name.ends_with(".desktop") {
                            let data = read_to_string(path).unwrap_or_else(|err|{
                                eprintln!("Failed to read desktop file: {name}: {err}");
                                exit(1)
                            });
                            appname = data.split("\n").filter_map(|string| {
                                if string.starts_with("Exec=") {
                                    Some(string.replace("Exec=", "").split_whitespace().next().unwrap_or("").into())
                                } else {None}
                            }).next().unwrap_or_else(||"".into())
                        }
                    }
                }
            }
        }

        if appname.is_empty() {
            appname = read_to_string(appname_file).unwrap_or_else(|err|{
                eprintln!("Failed to read .app file: {appname_file}: {err}");
                exit(1)
            })
        }

        if let Some(name) = appname.trim().split("\n").next() {
            appname = basename(name)
            .replace("'", "").replace("\"", "")
        } else {
            eprintln!("Failed to get app name: {appname_file}");
            exit(1)
        }
        let app = &format!("{bin_dir}/{appname}");

        add_to_env("PATH", bin_dir);
        if get_env_var("ARGV0").is_empty() {
            env::set_var("ARGV0", &arg0)
        }
        env::set_var("APPDIR", &sharun_dir);

        let err = Command::new(app)
            .envs(env::vars())
            .args(exec_args)
            .exec();
        eprintln!("Failed to run App: {app}: {err}");
        exit(1)
    }
    let bin = format!("{shared_bin}/{bin_name}");

    let is_elf32_bin = is_elf32(&bin).unwrap_or_else(|err|{
        eprintln!("Failed to check ELF class: {bin}: {err}");
        exit(1)
    });

    let mut library_path = if is_elf32_bin {
        shared_lib32
    } else {
        shared_lib
    };

    let unset_envs = read_dotenv(&sharun_dir);

    let interpreter = get_interpreter(&library_path).unwrap_or_else(|_|{
        eprintln!("Interpreter not found!");
        exit(1)
    });

    let working_dir = &get_env_var("SHARUN_WORKING_DIR");
    if !working_dir.is_empty() {
        env::set_current_dir(working_dir).unwrap_or_else(|err|{
            eprintln!("Failed to change working directory: {working_dir}: {err}");
            exit(1)
        });
        env::remove_var("SHARUN_WORKING_DIR")
    }

    let lib_path_file = &format!("{library_path}/lib.path");
    if !Path::new(lib_path_file).exists() && is_writable(&library_path) {
        gen_library_path(&library_path, lib_path_file)
    }

    add_to_env("PATH", bin_dir);

    if let Ok(lib_path_data) = read_to_string(lib_path_file) {
        let lib_path_data = lib_path_data.trim();
        let dirs: HashSet<&str> = lib_path_data.split("\n").map(|string|{
            string.split("/").nth(1).unwrap_or("")
        }).collect();
        for dir in dirs {
            let dir_path = &format!("{library_path}/{dir}");
            if dir.starts_with("python") {
                add_to_env("PYTHONHOME", &sharun_dir);
                env::set_var("PYTHONDONTWRITEBYTECODE", "1")
            }
            if dir.starts_with("perl") {
                add_to_env("PERLLIB", dir_path)
            }
            if dir == "gconv" {
                add_to_env("GCONV_PATH", dir_path)
            }
            if dir == "gio" {
                let modules = &format!("{dir_path}/modules");
                if Path::new(modules).exists() {
                    env::set_var("GIO_MODULE_DIR", modules)
                }
            }
            if dir == "dri" {
                env::set_var("LIBGL_DRIVERS_PATH", dir_path)
            }
            if dir.starts_with("spa-") {
                env::set_var("SPA_PLUGIN_DIR", dir_path)
            }
            if dir.starts_with("pipewire-") {
                env::set_var("PIPEWIRE_MODULE_DIR", dir_path)
            }
            if dir.starts_with("gtk-") {
                add_to_env("GTK_PATH", dir_path);
                env::set_var("GTK_EXE_PREFIX", &sharun_dir);
                env::set_var("GTK_DATA_PREFIX", &sharun_dir);
                for entry in WalkDir::new(dir_path).into_iter().flatten() {
                    let path = entry.path();
                    if path.is_file() && entry.file_name().to_string_lossy() == "immodules.cache" {
                        env::set_var("GTK_IM_MODULE_FILE", path);
                        break
                    }
                }
            }
            if dir.starts_with("qt") {
                let qt_conf = &format!("{bin_dir}/qt.conf");
                let plugins = &format!("{dir_path}/plugins");
                if Path::new(plugins).exists() && ! Path::new(qt_conf).exists() {
                    add_to_env("QT_PLUGIN_PATH", plugins)
                }
            }
            if dir.starts_with("babl-") {
                env::set_var("BABL_PATH", dir_path)
            }
            if dir.starts_with("gegl-") {
                env::set_var("GEGL_PATH", dir_path)
            }
            if dir == "gimp" {
                let plugins = &format!("{dir_path}/2.0");
                if Path::new(plugins).exists() {
                    env::set_var("GIMP2_PLUGINDIR", plugins)
                }
            }
            if dir == "libdecor" {
                let plugins = &format!("{dir_path}/plugins-1");
                if Path::new(plugins).exists() {
                    env::set_var("LIBDECOR_PLUGIN_DIR", plugins)
                }
            }
            if dir.starts_with("tcl") && Path::new(&format!("{dir_path}/msgs")).exists() {
                add_to_env("TCL_LIBRARY", dir_path);
                let tk = &format!("{library_path}/{}", dir.replace("tcl", "tk"));
                if Path::new(&tk).exists() {
                    add_to_env("TK_LIBRARY", tk)
                }
            }
            if dir.starts_with("gstreamer-") {
                add_to_env("GST_PLUGIN_PATH", dir_path);
                add_to_env("GST_PLUGIN_SYSTEM_PATH", dir_path);
                add_to_env("GST_PLUGIN_SYSTEM_PATH_1_0", dir_path);
                let gst_scanner = &format!("{dir_path}/gst-plugin-scanner");
                if Path::new(gst_scanner).exists() {
                    env::set_var("GST_PLUGIN_SCANNER", gst_scanner)
                }
            }
            if dir.starts_with("gdk-pixbuf-") {
                let mut is_loaders = false;
                let mut is_loaders_cache = false;
                for entry in WalkDir::new(dir_path).into_iter().flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy();
                    if name == "loaders" && path.is_dir() {
                        env::set_var("GDK_PIXBUF_MODULEDIR", path);
                        is_loaders = true
                    }
                    if name == "loaders.cache" && path.is_file() {
                        env::set_var("GDK_PIXBUF_MODULE_FILE", path);
                        is_loaders_cache = true
                    }
                    if is_loaders && is_loaders_cache {
                        break
                    }
                }
            }
        }
        library_path = lib_path_data
            .replace("\n", ":")
            .replace("+", &library_path)
    }

    let share_dir = PathBuf::from(format!("{sharun_dir}/share"));
    if share_dir.exists() {
        if let Ok(dir) = share_dir.read_dir() {
            add_to_env("XDG_DATA_DIRS", "/usr/local/share");
            add_to_env("XDG_DATA_DIRS", "/usr/share");
            add_to_env("XDG_DATA_DIRS", &share_dir);
            for entry in dir.flatten() {
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    let name = entry.file_name();
                    match name.to_str().unwrap() {
                        "glvnd" =>  {
                            let egl_vendor = &entry_path.join("egl_vendor.d");
                            if egl_vendor.exists() {
                                add_to_env("__EGL_VENDOR_LIBRARY_DIRS", "/usr/share/glvnd/egl_vendor.d");
                                add_to_env("__EGL_VENDOR_LIBRARY_DIRS", egl_vendor)
                            }
                        }
                        "vulkan" =>  {
                            let icd = &entry_path.join("icd.d");
                            if icd.exists() {
                                add_to_env("VK_DRIVER_FILES", "/usr/share/vulkan/icd.d");
                                add_to_env("VK_DRIVER_FILES", icd)
                            }
                        }
                        "X11" =>  {
                            let xkb = &entry_path.join("xkb");
                            if xkb.exists() {
                                env::set_var("XKB_CONFIG_ROOT", xkb)
                            }
                        }
                        "glib-2.0" =>  {
                            let schemas = &entry_path.join("schemas");
                            if schemas.exists() {
                                add_to_env("GSETTINGS_SCHEMA_DIR", "/usr/share/glib-2.0/schemas");
                                add_to_env("GSETTINGS_SCHEMA_DIR", schemas)
                            }
                        }
                        "gimp" =>  {
                            let gimp2_datadir = &entry_path.join("2.0");
                            if gimp2_datadir.exists() {
                                env::set_var("GIMP2_DATADIR",gimp2_datadir)
                            }
                        }
                        "terminfo" =>  {
                            env::set_var("TERMINFO",entry_path)
                        }
                        "file" =>  {
                            let magic_file = &entry_path.join("misc/magic.mgc");
                            if magic_file.exists() {
                                env::set_var("MAGIC", magic_file)
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let etc_dir = PathBuf::from(format!("{sharun_dir}/etc"));
    if etc_dir.exists() {
        if let Ok(dir) = etc_dir.read_dir() {
            for entry in dir.flatten() {
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    let name = entry.file_name();
                    match name.to_str().unwrap() {
                        "fonts" => {
                            let fonts_conf = entry_path.join("fonts.conf");
                            if fonts_conf.exists() {
                                env::set_var("FONTCONFIG_FILE", fonts_conf)
                            }
                        }
                        "gimp" => {
                            let gimp2_sysconfdir = entry_path.join("2.0");
                            if gimp2_sysconfdir.exists() {
                                env::set_var("GIMP2_SYSCONFDIR", gimp2_sysconfdir)
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    for var_name in unset_envs {
        env::remove_var(var_name)
    }

    let envs: Vec<CString> = env::vars()
        .map(|(key, value)| CString::new(
            format!("{}={}", key, value)
    ).unwrap()).collect();

    let is_pyinstaller_elf = is_elf_section(&bin, "pydata").unwrap_or(false);

    let mut interpreter_args = vec![
        CString::from_str(&interpreter.to_string_lossy()).unwrap(),
        CString::new("--library-path").unwrap(),
        CString::new(library_path).unwrap(),
        CString::new("--argv0").unwrap(),
    ];

    if is_pyinstaller_elf {
        interpreter_args.push(CString::new(&*bin).unwrap())
    } else {
        interpreter_args.push(CString::new(arg0_path.to_str().unwrap()).unwrap())
    }

    let preload_path = PathBuf::from(format!("{sharun_dir}/.preload"));
    if preload_path.exists() {
        let data = read_to_string(&preload_path).unwrap_or_else(|err|{
            eprintln!("Failed to read .preload file: {}: {err}", preload_path.display());
            exit(1)
        });
        let mut preload: Vec<String> = vec![];
        for string in data.trim().split("\n") {
            preload.push(string.trim().into());            
        }
        if !preload.is_empty() {
            interpreter_args.append(&mut vec![
                CString::new("--preload").unwrap(),
                CString::new(preload.join(" ")).unwrap()
            ])
        }
    }

    interpreter_args.push(CString::new(&*bin).unwrap());
    for arg in exec_args {
        interpreter_args.push(CString::from_str(&arg).unwrap())
    }

    if is_pyinstaller_elf {
        let mut interpreter_args: Vec<*const i8> = interpreter_args.iter().map(|s| s.as_ptr()).collect();
        let mut envs: Vec<*const i8> = envs.iter().map(|s| s.as_ptr()).collect();
        interpreter_args.push(std::ptr::null());
        envs.push(std::ptr::null());
        unsafe { execve(
            interpreter_args[0],
            interpreter_args.as_ptr(),
            envs.as_ptr(),
        ); }
    } else {
        userland_execve::exec(
            interpreter.as_path(),
            &interpreter_args,
            &envs,
        )
    }
}
