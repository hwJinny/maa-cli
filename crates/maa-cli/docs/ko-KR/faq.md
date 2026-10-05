# 자주 묻는 질문

## 1. macOS에서 `$HOME/.config/maa`를 설정 파일 디렉토리로 사용하려면 어떻게 해야 하나요?

Rust 라이브러리 [Directories](https://github.com/dirs-dev/directories-rs/)가 macOS에서 Apple 스타일의 디렉토리를 기본으로 사용하기 때문에, maa-cli도 기본적으로 Apple 스타일의 설정 디렉토리를 사용합니다. 하지만 명령줄 프로그램의 경우 XDG 스타일 디렉토리가 더 적합합니다. XDG 스타일 디렉토리를 사용하려면 `XDG_CONFIG_HOME` 환경 변수를 설정하면 됩니다. 예를 들어, `"export XDG_CONFIG_HOME="$HOME/.config"` 명령어를 사용하면 maa-cli가 XDG 스타일 설정 디렉토리를 사용하게 됩니다. 환경 변수를 설정하지 않고 XDG 스타일 설정 디렉토리를 사용하려면, 다음 명령어로 심볼릭 링크를 생성할 수 있습니다:

```bash
mkdir -p "$HOME/.config/maa"
ln -s "$HOME/.config/maa" "$(maa dir config)"
```

## 2. 빈 `MAA_CONFIG_DIR`가 빈 경로를 가리키지 않는 이유는 무엇인가요?

`MAA_DATA_DIR`, `MAA_STATE_DIR`, `MAA_CACHE_DIR`, `MAA_CONFIG_DIR`는 비어 있지 않은 값만 적용됩니다. `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`도 같습니다. 따라서 `export MAA_CONFIG_DIR=`는 변수를 비운 것과 같습니다. maa-cli는 비어 있지 않은 `XDG_CONFIG_HOME/maa`로 돌아간 뒤, 그것도 없으면 플랫폼 기본 경로를 사용합니다. 공백만 있는 값은 빈 문자열이 아니므로 적힌 경로 그대로 사용합니다.
