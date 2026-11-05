// pico's replacement for clang's FrontendTool library (lib/FrontendTool/ExecuteCompilerInvocation.cpp):
// the same entry point, `clang::ExecuteCompilerInvocation`, for the actions pico runs (objects,
// assembly, preprocessing, the AST dump `c:` imports read). Leaving FrontendTool out keeps the
// static analyzer (and the z3 solver it pulls into LLVM's Support library), ExtractAPI, the
// rewriters and the plugins out of the compiler.
#include "clang/Basic/DiagnosticFrontend.h"
#include "clang/CodeGen/CodeGenAction.h"
#include "clang/Frontend/CompilerInstance.h"
#include "clang/Frontend/FrontendActions.h"
#include "clang/FrontendTool/Utils.h"
#include "clang/Options/Options.h"
#include "llvm/Option/OptTable.h"
#include "llvm/Support/BuryPointer.h"
#include "llvm/Support/CommandLine.h"

#include <memory>

using namespace clang;

static std::unique_ptr<FrontendAction> createAction(CompilerInstance &CI) {
    using namespace clang::frontend;
    switch (CI.getFrontendOpts().ProgramAction) {
    case ASTDeclList: return std::make_unique<ASTDeclListAction>();
    case ASTDump: return std::make_unique<ASTDumpAction>();
    case ASTPrint: return std::make_unique<ASTPrintAction>();
    case DumpCompilerOptions: return std::make_unique<DumpCompilerOptionsAction>();
    case DumpRawTokens: return std::make_unique<DumpRawTokensAction>();
    case DumpTokens: return std::make_unique<DumpTokensAction>();
    case EmitAssembly: return std::make_unique<EmitAssemblyAction>();
    case EmitBC: return std::make_unique<EmitBCAction>();
    case EmitLLVM: return std::make_unique<EmitLLVMAction>();
    case EmitLLVMOnly: return std::make_unique<EmitLLVMOnlyAction>();
    case EmitCodeGenOnly: return std::make_unique<EmitCodeGenOnlyAction>();
    case EmitObj: return std::make_unique<EmitObjAction>();
    case InitOnly: return std::make_unique<InitOnlyAction>();
    case ParseSyntaxOnly: return std::make_unique<SyntaxOnlyAction>();
    case PrintPreprocessedInput:
        if (!CI.getPreprocessorOutputOpts().RewriteIncludes &&
            !CI.getPreprocessorOutputOpts().RewriteImports)
            return std::make_unique<PrintPreprocessedAction>();
        break;
    case RunPreprocessorOnly: return std::make_unique<PreprocessOnlyAction>();
    default: break;
    }
    CI.getDiagnostics().Report(diag::err_fe_action_not_available) << "this action (in pico's clang)";
    return nullptr;
}

namespace clang {

bool ExecuteCompilerInvocation(CompilerInstance *Clang) {
    unsigned errorsBefore = Clang->getDiagnostics().getNumErrors();
    if (Clang->getFrontendOpts().ShowHelp) {
        getDriverOptTable().printHelp(llvm::outs(), "clang -cc1 [options] file...",
                                      "LLVM 'Clang' Compiler: http://clang.llvm.org",
                                      /*ShowHidden=*/false, /*ShowAllAliases=*/false,
                                      llvm::opt::Visibility(options::CC1Option));
        return true;
    }
    if (Clang->getFrontendOpts().ShowVersion) {
        llvm::cl::PrintVersionMessage();
        return true;
    }
    // -mllvm options.
    const auto &llvmArgs = Clang->getFrontendOpts().LLVMArgs;
    if (!llvmArgs.empty()) {
        std::vector<const char *> args{"clang (LLVM option parsing)"};
        for (const auto &a : llvmArgs)
            args.push_back(a.c_str());
        args.push_back(nullptr);
        llvm::cl::ParseCommandLineOptions(int(args.size() - 1), args.data(), "", nullptr,
                                          &Clang->getVirtualFileSystem());
    }
    if (Clang->getDiagnostics().getNumErrors() != errorsBefore)
        return false;
    std::unique_ptr<FrontendAction> act = createAction(*Clang);
    if (!act)
        return false;
    bool ok = Clang->ExecuteAction(*act);
    if (Clang->getFrontendOpts().DisableFree)
        llvm::BuryPointer(std::move(act));
    return ok;
}

} // namespace clang
