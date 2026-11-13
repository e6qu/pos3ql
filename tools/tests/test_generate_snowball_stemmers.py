import importlib.util
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "generate_snowball_stemmers", ROOT / "tools" / "generate-snowball-stemmers.py"
)
GENERATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GENERATOR)

SOURCE = """
static const symbol s_0_0[2] = { 'e', 'd' };
static const symbol s_0_1[3] = { 'i', 'n', 'g' };
static const struct among a_0[2] =
{
{ 2, s_0_0, -1, 1, 0},
{ 3, s_0_1, -1, 2, r_check}
};
static const unsigned char g_v[] = { 17, 65, 16, 1 };
static const symbol s_0[] = { 'e' };
static int r_check(struct SN_env * z) {
    return z->I[0] <= z->c;
}
extern int sample_UTF_8_stem(struct SN_env * z) {
    int among_var;
    z->lb = z->c; z->c = z->l;
    {   int m1 = z->l - z->c; (void)m1;
        z->ket = z->c;
        among_var = find_among_b(z, a_0, 2);
        if (!among_var) goto lab0;
        z->bra = z->c;
        switch (among_var) {
            case 1:
                {   int ret = slice_del(z);
                    if (ret < 0) return ret;
                }
                break;
            case 2:
                {   int ret = slice_from_s(z, 1, s_0);
                    if (ret < 0) return ret;
                }
                break;
        }
    lab0:
        z->c = z->l - m1;
    }
    while(1) {
        if (in_grouping_b_U(z, g_v, 97, 121, 0)) break;
    }
    z->c = z->lb;
    return 1;
}
extern struct SN_env * sample_UTF_8_create_env(void) { return SN_create_env(0, 1); }
"""


class SnowballTranslationTests(unittest.TestCase):
    def test_translates_control_flow_and_tables(self):
        rust = "\n".join(GENERATOR.Translator("sample", SOURCE).render())
        self.assertIn("Some(r_check)", rust)
        self.assertIn("'lab0: {", rust)
        self.assertIn("break 'lab0;", rust)
        self.assertIn("match among_var {", rust)
        self.assertIn("'loop2: loop {", rust)
        self.assertIn("z.in_grouping_b_utf8(G_V, 97, 121, false) != 0", rust)
        self.assertIn("const S_0: &[u8] = b\"e\";", rust)
        self.assertIn("pub(super) fn stem(z: &mut Env) -> i32 {", rust)

    def test_rejects_a_backward_goto(self):
        source = SOURCE.replace("    lab0:\n        z->c = z->l - m1;", "        z->c = z->l - m1;")
        source = source.replace("    z->c = z->lb;\n", "lab0:\n    z->c = z->lb;\n    goto lab0;\n")
        with self.assertRaises(GENERATOR.TranslateError):
            GENERATOR.Translator("sample", source).render()


if __name__ == "__main__":
    unittest.main()
