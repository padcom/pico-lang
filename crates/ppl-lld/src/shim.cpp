// What ppl needs from lld and LLVM's COFF support: run a linker in-process (ELF, or COFF for
// Windows), and write a Windows import library from a module-definition (.def) file.
#include "lld/Common/Driver.h"
#include "llvm/Object/COFFImportFile.h"
#include "llvm/Object/COFFModuleDefinition.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/raw_ostream.h"

#include <string>
#include <vector>

LLD_HAS_DRIVER(elf)
LLD_HAS_DRIVER(coff)
LLD_HAS_DRIVER(mingw)

static char *copy_out(const std::string &text) {
    char *out = new char[text.size() + 1];
    text.copy(out, text.size());
    out[text.size()] = 0;
    return out;
}

// Links with `argv`; argv[0] names the flavor, as for the executables (`ld.lld`, `lld-link`).
// Returns lld's exit code; its messages go to `*out` (freed with ppl_lld_free).
extern "C" int ppl_lld_link(const char *const *argv, int argc, char **out, int *can_run_again) {
    std::vector<const char *> args(argv, argv + argc);
    std::string text;
    llvm::raw_string_ostream os(text);
    lld::Result r = lld::lldMain(args, os, os,
                                 {{lld::Gnu, &lld::elf::link},
                                  {lld::WinLink, &lld::coff::link},
                                  {lld::MinGW, &lld::mingw::link}});
    os.flush();
    *out = copy_out(text);
    *can_run_again = r.canRunAgain;
    return r.retCode;
}

// Writes the import library `lib_path` for the (preprocessed) MinGW .def file `def_path`,
// for COFF machine `machine`, the way llvm-dlltool does (with --kill-at). Returns 0, or 1 with
// the message in `*out`.
extern "C" int ppl_write_import_lib(const char *def_path, unsigned machine, const char *lib_path,
                                    char **out) {
    using namespace llvm;
    auto m = static_cast<COFF::MachineTypes>(machine);
    auto buf = MemoryBuffer::getFile(def_path);
    if (!buf) {
        *out = copy_out("cannot read " + std::string(def_path));
        return 1;
    }
    auto def = object::parseCOFFModuleDefinition(**buf, m, /*MingwDef=*/true);
    if (!def) {
        *out = copy_out(toString(def.takeError()));
        return 1;
    }
    for (object::COFFShortExport &e : def->Exports) {
        // `ExtName = Name`: only the external name matters for an import library.
        if (!e.ExtName.empty()) {
            e.Name = e.ExtName;
            e.ExtName.clear();
        }
        // --kill-at: drop stdcall decoration (only i386 has it; harmless elsewhere).
        if (!e.ImportName.empty() || (!e.Name.empty() && e.Name[0] == '?')) continue;
        if (m == COFF::IMAGE_FILE_MACHINE_I386) e.SymbolName = e.Name;
        e.Name = e.Name.substr(0, e.Name.find('@', 1));
    }
    if (Error err = object::writeImportLibrary(def->OutputFile, lib_path, def->Exports, m,
                                               /*MinGW=*/true)) {
        *out = copy_out(toString(std::move(err)));
        return 1;
    }
    *out = copy_out("");
    return 0;
}

extern "C" void ppl_lld_free(char *p) { delete[] p; }
