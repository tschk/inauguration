// Dump function / decompiler metrics for inauguration antidecomp smoke.
//@category Inauguration
//@menupath
//@toolbar

import java.util.ArrayList;
import java.util.List;

import ghidra.app.decompiler.DecompInterface;
import ghidra.app.decompiler.DecompileResults;
import ghidra.app.script.GhidraScript;
import ghidra.program.model.listing.Function;
import ghidra.program.model.listing.FunctionIterator;
import ghidra.program.model.listing.FunctionManager;
import ghidra.program.model.symbol.SourceType;

public class GhidraDumpFuncs extends GhidraScript {

	@Override
	public void run() throws Exception {
		String progName = currentProgram.getName();
		emit("GHIDRA_PROGRAM=" + progName);

		FunctionManager fm = currentProgram.getFunctionManager();
		List<Function> funcs = new ArrayList<>();
		FunctionIterator it = fm.getFunctions(true);
		while (it.hasNext()) {
			funcs.add(it.next());
		}
		emit("GHIDRA_FUNC_COUNT=" + funcs.size());

		int named = 0;
		int hashed = 0;
		int defaulted = 0;
		StringBuilder sb = new StringBuilder();
		for (Function f : funcs) {
			String n = f.getName();
			SourceType src = f.getSymbol().getSource();
			sb.setLength(0);
			sb.append("GHIDRA_FUNC name=").append(n)
			  .append(" entry=").append(f.getEntryPoint())
			  .append(" source=").append(src);
			emit(sb.toString());
			if (n.startsWith("_H") && n.length() > 2) {
				hashed++;
			}
			else if (n.startsWith("FUN_") || n.startsWith("thunk_FUN_")) {
				defaulted++;
			}
			else {
				named++;
			}
		}
		emit("GHIDRA_NAMED_COUNT=" + named);
		emit("GHIDRA_HASHED_COUNT=" + hashed);
		emit("GHIDRA_DEFAULTED_COUNT=" + defaulted);

		int ok = 0;
		int fail = 0;
		int totalChars = 0;
		DecompInterface decomp = new DecompInterface();
		try {
			decomp.openProgram(currentProgram);
			int limit = Math.min(12, funcs.size());
			for (int i = 0; i < limit; i++) {
				Function f = funcs.get(i);
				DecompileResults res = decomp.decompileFunction(f, 30, monitor);
				if (res != null && res.decompileCompleted() && res.getDecompiledFunction() != null) {
					String c = res.getDecompiledFunction().getC();
					totalChars += (c == null) ? 0 : c.length();
					ok++;
				}
				else {
					fail++;
				}
			}
		}
		catch (Exception e) {
			emit("GHIDRA_DECOMP_ERROR=" + e.getMessage());
		}
		finally {
			decomp.dispose();
		}
		emit("GHIDRA_DECOMP_OK=" + ok);
		emit("GHIDRA_DECOMP_FAIL=" + fail);
		emit("GHIDRA_DECOMP_CHARS=" + totalChars);
	}

	private void emit(String line) {
		// System.out so headless log grep '^GHIDRA_' works without script prefix.
		System.out.println(line);
	}
}
