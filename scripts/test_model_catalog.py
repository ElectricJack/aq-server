"""Picker metadata must not invent capabilities or include embedding models."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
spec=importlib.util.spec_from_file_location('catalog',Path(__file__).with_name('refresh-model-catalog.py'))
catalog=importlib.util.module_from_spec(spec)
spec.loader.exec_module(catalog)
class CatalogTests(unittest.TestCase):
 def test_missing_effort_metadata_keeps_default_without_fabricated_levels(self):
  data={'google':{'models':{'unknown':{'tool_call':True,'modalities':{'input':['text']}}}}}
  choices=catalog.public_choices(data,'2026-11-09T00:00:00Z')
  self.assertEqual([(c['reasoning'],c['options']) for c in choices],[('Provider default',{})])
  self.assertTrue(all(c['preference_only'] for c in choices))
 def test_exact_model_efforts_and_partial_budget_bounds_are_preserved(self):
  data={'google':{'models':{'level-model':{'tool_call':True,'reasoning_options':[{'type':'effort','values':['low','high']}]},'budget-model':{'tool_call':True,'reasoning_options':[{'type':'budget_tokens','min':128}]}}}}
  choices=catalog.public_choices(data,None)
  self.assertEqual({c['reasoning'] for c in choices if c['model']=='level-model'},{'low','high'})
  self.assertEqual([c['options'] for c in choices if c['model']=='budget-model'],[{'thinking_budget':128}])
 def test_non_tool_and_embedding_only_models_are_excluded(self):
  data={'google':{'models':{'embedding':{'tool_call':False},'image-only':{'tool_call':True,'modalities':{'input':['image']}}}}}
  self.assertEqual(catalog.public_choices(data,None),[])
 def test_atomic_cache_replacement_is_valid_json(self):
  import json
  with tempfile.TemporaryDirectory() as directory:
   path=Path(directory)/'catalog.json';catalog.atomic_json(path,[{'old':True}]);catalog.atomic_json(path,[{'new':True}])
   self.assertEqual(json.loads(path.read_text()),[{'new':True}])
   self.assertEqual(list(Path(directory).iterdir()),[path])
 def test_operator_metadata_path_remains_at_toml_root(self):
  from unittest.mock import patch
  import tomllib
  spec=importlib.util.spec_from_file_location('dev_stack',Path(__file__).with_name('dev-stack.py'))
  module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
  with tempfile.TemporaryDirectory() as directory:
   path=Path(directory)/'operator.toml';path.write_text('intelligence_preferences_file="/tmp/models.json"\n[intelligence]\nrevision=1\n')
   with patch.dict('os.environ',{'AQ_DEV_INTELLIGENCE_FILE':str(path)}):
    prefix,tables=module.intelligence_section()
   config=tomllib.loads(prefix+'[workers]\n'+tables)
   self.assertEqual(config['intelligence_preferences_file'],'/tmp/models.json')
   self.assertNotIn('intelligence_preferences_file',config['workers'])
if __name__=='__main__':unittest.main()
