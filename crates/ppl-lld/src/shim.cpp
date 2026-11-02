// The one entry point ppl needs from lld: run the ELF linker in-process.
#include "lld/Common/Driver.h"
#include "llvm/Support/raw_ostream.h"

#include <string>
#include <vector>

LLD_HAS_DRIVER(elf)

// Links with `argv` (argv[0] names the flavor, as for the `ld.lld` executable). Returns lld's
// exit code; its messages go to `*out` (allocated with new[], freed by ppl_lld_free).
extern "C" int ppl_lld_link(const char *const *argv, int argc, char **out, int *can_run_again) {
    std::vector<const char *> args(argv, argv + argc);
    std::string text;
    llvm::raw_string_ostream os(text);
    lld::Result r = lld::lldMain(args, os, os, {{lld::Gnu, &lld::elf::link}});
    os.flush();
    *out = new char[text.size() + 1];
    text.copy(*out, text.size());
    (*out)[text.size()] = 0;
    *can_run_again = r.canRunAgain;
    return r.retCode;
}

extern "C" void ppl_lld_free(char *p) { delete[] p; }
