# FAQs

## 1. How to use `$HOME/.config/maa` as the configuration directory on macOS?

Due to the Rust library [Directories](https://github.com/dirs-dev/directories-rs/) using Apple-style directories on macOS by default, maa-cli also uses Apple-style configuration directories by default. However, XDG-style directories are more suitable for command-line programs. If you want to use XDG-style directories, you can set the `XDG_CONFIG_HOME` environment variable, such as `export XDG_CONFIG_HOME="$HOME/.config"`, which will make maa-cli use XDG-style configuration directories. If you want to use XDG-style configuration directories but don't want to set environment variables, you can use the following command to create a symbolic link:

```bash
mkdir -p "$HOME/.config/maa"
ln -s "$HOME/.config/maa" "$(maa dir config)"
```

## 2. Why does an empty `MAA_CONFIG_DIR` not create an empty configuration path?

`MAA_DATA_DIR`, `MAA_STATE_DIR`, `MAA_CACHE_DIR`, and `MAA_CONFIG_DIR` accept only a non-empty value. The same rule applies to `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, and `XDG_CONFIG_HOME`. `export MAA_CONFIG_DIR=` is therefore the same as leaving the variable unset: maa-cli falls back to a non-empty `XDG_CONFIG_HOME/maa`, then to the platform default. A path that is only spaces is not empty and is used as written.
