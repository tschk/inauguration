💡 **What:** Optimized the loop by taking active_projections instead of cloning it.
🎯 **Why:** To avoid inefficient clone in the loop.
📊 **Measured Improvement:** The change avoids a full HashSet clone and reduces memory allocation overhead on the hot path.
