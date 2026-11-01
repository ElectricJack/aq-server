# AQ local patched PNG source

Verified official crates.io png 0.18.1 archive, followed only by upstream commit 8330c2c6fc91c56fb8c4bb0cd6ea65994027f15e (five added lines), retained as AQ-UPSTREAM.patch. See AQ-PROVENANCE.json for original archive and patched-file hashes. Original licenses/source/manifest remain present.

Used by publish=false workspace code only. A workspace Cargo patch does not propagate to a separately published consumer; publishing requires downstream vendoring/patch configuration or a qualified fixed upstream release. Do not claim standalone published-package qualification.
