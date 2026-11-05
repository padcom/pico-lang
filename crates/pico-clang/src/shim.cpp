// What pico needs from clang, linked in (decision 298): clang's driver (`pico clang …` runs it,
// with cc1 and the assembler in the same process), and LLVM's archive writer (for
// `pico build --lib static`, in place of llvm-ar).
#include "llvm/ADT/ArrayRef.h"
#include "llvm/Object/ArchiveWriter.h"
#include "llvm/Support/Error.h"
#include "llvm/Support/LLVMDriver.h"
#include "llvm/Support/raw_ostream.h"

#include <string>
#include <vector>

// vendor/driver.cpp
int clang_main(int Argc, char **Argv, const llvm::ToolContext &ToolContext);

// vendor/driver.cpp calls it for `-cc1gen-reproducer`, which pico's clang doesn't support.
int cc1gen_reproducer_main(llvm::ArrayRef<const char *>, const char *, void *,
                           const llvm::ToolContext &) {
    llvm::errs() << "error: crash reproducers aren't supported by pico's clang\n";
    return 1;
}

// Runs clang's driver: `argv[0]` is the program name it reports (`clang`), `exe` the path of
// pico (where the driver would start cc1 if it ran it in a process of its own: `exe clang -cc1`).
extern "C" int pico_clang_main(int argc, char **argv, const char *exe) {
    llvm::ToolContext ctx{exe, "clang", true};
    return clang_main(argc, argv, ctx);
}

static char *copy_out(const std::string &text) {
    char *out = new char[text.size() + 1];
    text.copy(out, text.size());
    out[text.size()] = 0;
    return out;
}

extern "C" void pico_clang_free(char *p) { delete[] p; }

// Writes a static library (GNU format, with a symbol table, deterministic) of `members`.
// Returns 0, or 1 with the error in `*err` (freed with pico_clang_free).
extern "C" int pico_write_archive(const char *out, const char *const *members, int n, char **err) {
    std::vector<llvm::NewArchiveMember> ms;
    for (int i = 0; i < n; i++) {
        auto m = llvm::NewArchiveMember::getFile(members[i], /*Deterministic=*/true);
        if (!m) {
            *err = copy_out(std::string(members[i]) + ": " + llvm::toString(m.takeError()));
            return 1;
        }
        ms.push_back(std::move(*m));
    }
    if (llvm::Error e = llvm::writeArchive(out, ms, llvm::SymtabWritingMode::NormalSymtab,
                                           llvm::object::Archive::K_GNU,
                                           /*Deterministic=*/true, /*Thin=*/false)) {
        *err = copy_out(llvm::toString(std::move(e)));
        return 1;
    }
    return 0;
}
