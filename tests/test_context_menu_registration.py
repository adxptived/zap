from pathlib import Path


REGISTER_SCRIPT = Path(__file__).resolve().parents[1] / "dist" / "register-context-menu.py"


def test_context_menu_uses_document_model_for_batched_legacy_invocation():
    source = REGISTER_SCRIPT.read_text(encoding="utf-8")

    assert 'set_value(menu_root, "MultiSelectModel", "Document")' in source
    assert 'set_value(item_path, "MultiSelectModel", "Document")' in source


def test_context_menu_raises_explorer_multi_invoke_limit():
    source = REGISTER_SCRIPT.read_text(encoding="utf-8")

    assert '"MultipleInvokePromptMinimum"' in source
    assert "MAX_CONTEXT_MENU_SELECTIONS" in source


def test_context_menu_makes_gui_delete_the_primary_item():
    source = REGISTER_SCRIPT.read_text(encoding="utf-8")

    gui_item = source.index('"delete-dialog"')
    zap_delete = source.index('"zap-delete"')

    assert gui_item < zap_delete
    assert '"Delete..."' in source
    assert '"Zap Delete"' in source
    assert '"Preview in terminal"' not in source
    assert '"preview-terminal"' not in source
    assert '"Delete in terminal"' not in source
    assert '"delete-terminal"' not in source
    assert "--silent --yes" in source
