# Vendored from llvm-project

`driver.cpp`, `cc1_main.cpp` and `cc1as_main.cpp` are clang's driver (`clang/tools/driver/`) from
llvm-project `llvmorg-23.1.3`, unmodified. They're only part of the clang executable, not of
clang's libraries, so pico compiles them itself (decision 298). `../src/frontend.cpp` replaces
clang's FrontendTool library, and `../src/shim.cpp` provides the rest (`cc1gen_reproducer_main`).

License: Apache-2.0 WITH LLVM-exception (`LICENSE.TXT`).

When pico moves to another LLVM version, update them from the matching tag:

```sh
for f in driver.cpp cc1_main.cpp cc1as_main.cpp; do
  gh api "repos/llvm/llvm-project/contents/clang/tools/driver/$f?ref=llvmorg-<version>" \
    --jq .content | base64 -d > "$f"
done
```
