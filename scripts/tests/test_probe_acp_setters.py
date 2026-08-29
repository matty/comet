import importlib.util
import subprocess
import sys
import unittest
from pathlib import Path


SCRIPT_PATH = Path(__file__).parents[1] / "probe-acp-setters.py"
SPEC = importlib.util.spec_from_file_location("probe_acp_setters", SCRIPT_PATH)
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)


class CandidatesModelIdTests(unittest.TestCase):
    """D117: the probe's model id is a --model flag, not a hardcoded literal."""

    def test_default_model_id_is_the_placeholder(self):
        # No --model given: a run without the flag must behave exactly as it
        # did before the flag existed, i.e. still probe the placeholder. This
        # goes through the parser on purpose -- asserting on DEFAULT_MODEL_ID
        # alone would not notice argparse being wired to a different default.
        parsed = probe.build_parser().parse_args(["--exe", "grok"])
        self.assertEqual(parsed.model, "REPLACE_WITH_A_REAL_MODEL_ID")
        built = probe.candidates(parsed.model)
        self.assertEqual(built[0], ("session/set_model", {"modelId": "REPLACE_WITH_A_REAL_MODEL_ID"}))
        self.assertEqual(
            built[1],
            ("session/set_config_option", {"configId": "model", "value": "REPLACE_WITH_A_REAL_MODEL_ID"}),
        )

    def test_documented_example_command_lines_actually_set_the_model(self):
        # --args is argparse.REMAINDER, so it swallows everything after it.
        # A --model written after --args is silently handed to the agent as a
        # launch arg while the probe keeps the placeholder -- indistinguishable
        # from the agent rejecting a real id. Every usage line in the module
        # docstring that names --model has to parse to that model.
        lines = [
            line.strip()
            for line in probe.__doc__.splitlines()
            if line.strip().startswith("python scripts/probe-acp-setters.py")
        ]
        examples = [line for line in lines if "--model" in line]
        self.assertTrue(examples, "no documented --model example to check")
        for line in examples:
            argv = line.split()[2:]
            parsed = probe.build_parser().parse_args(argv)
            self.assertNotEqual(
                parsed.model,
                probe.DEFAULT_MODEL_ID,
                f"--model is swallowed by --args in the documented example: {line}",
            )
            self.assertNotIn("--model", parsed.args, line)

    def test_explicit_model_id_flows_into_the_model_setting_candidates(self):
        built = probe.candidates("grok-4-fast")
        self.assertEqual(built[0], ("session/set_model", {"modelId": "grok-4-fast"}))
        self.assertEqual(
            built[1],
            ("session/set_config_option", {"configId": "model", "value": "grok-4-fast"}),
        )
        # The mode candidates never carry a model id and must be untouched.
        self.assertEqual(built[2], ("session/set_mode", {"modeId": "low"}))
        self.assertEqual(built[3], ("session/set_mode", {"modeId": "not-a-real-mode-xyz"}))

    def test_help_documents_the_model_flag_with_a_real_example(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT_PATH), "--help"],
            capture_output=True,
            text=True,
            check=True,
        )
        # Collapse whitespace first: argparse re-wraps the options block to
        # the terminal width, so a raw substring match on the flag's own help
        # text breaks whenever COLUMNS is narrower than the phrase.
        rendered = " ".join(result.stdout.split())
        self.assertIn("--model MODEL", rendered)
        self.assertIn("model id to probe session/set_model", rendered)
        # A real value has to look nothing like the placeholder, else a
        # reader following --help would just paste the default back in.
        self.assertIn("grok-4-fast", rendered)


if __name__ == "__main__":
    unittest.main()
