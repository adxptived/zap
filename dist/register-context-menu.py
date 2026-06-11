import os
import sys
import winreg


BIN_DIR = os.path.dirname(sys.executable if getattr(sys, "frozen", False) else os.path.abspath(__file__))
EXE_PATH = os.path.join(BIN_DIR, "zap.exe")
ZAPW_PATH = os.path.join(BIN_DIR, "zapw.exe")
ZAPG_PATH = os.path.join(BIN_DIR, "zapg.exe")
ICON_PATH = os.path.join(BIN_DIR, "zap.ico")
CLASSES_ROOT = r"Software\Classes"
EXPLORER_ROOT = r"Software\Microsoft\Windows\CurrentVersion\Explorer"
OBJECT_MENU_ROOT = r"AllFilesystemObjects\shell\zap"
BACKGROUND_MENU_ROOT = r"Directory\Background\shell\zap"
MAX_CONTEXT_MENU_SELECTIONS = 10000
OLD_ROOTS = [
    r"Directory\shell\turbo-delete",
    r"Directory\Background\shell\turbo-delete",
    r"*\shell\turbo-delete",
    r"AllFilesystemObjects\shell\turbo-delete",
    r"Directory\shell\zap",
    BACKGROUND_MENU_ROOT,
    r"*\shell\zap",
    OBJECT_MENU_ROOT,
]


def key_path(path):
    return CLASSES_ROOT + "\\" + path


def delete_tree(root, path, ignore_permission=False):
    try:
        with winreg.OpenKey(root, path, 0, winreg.KEY_READ | winreg.KEY_WRITE) as key:
            while True:
                try:
                    child = winreg.EnumKey(key, 0)
                except OSError:
                    break
                delete_tree(root, path + "\\" + child, ignore_permission)
        winreg.DeleteKey(root, path)
    except FileNotFoundError:
        pass
    except OSError as err:
        if not ignore_permission or getattr(err, "winerror", None) != 5:
            raise


def create_key(path):
    try:
        key = winreg.CreateKey(winreg.HKEY_CURRENT_USER, key_path(path))
        key.Close()
    except OSError as err:
        raise OSError(f"failed to create HKCU\\{key_path(path)}: {err}") from err


def set_value(path, name, value):
    try:
        with winreg.OpenKey(
            winreg.HKEY_CURRENT_USER,
            key_path(path),
            0,
            winreg.KEY_SET_VALUE,
        ) as key:
            winreg.SetValueEx(key, name, 0, winreg.REG_SZ, value)
    except OSError as err:
        label = "(default)" if name == "" else name
        raise OSError(f"failed to set HKCU\\{key_path(path)} [{label}]: {err}") from err


def set_dword(root_path, name, value):
    try:
        with winreg.CreateKey(winreg.HKEY_CURRENT_USER, root_path) as key:
            winreg.SetValueEx(key, name, 0, winreg.REG_DWORD, value)
    except OSError as err:
        raise OSError(f"failed to set HKCU\\{root_path} [{name}]: {err}") from err


def add_menu_item(menu_root, name, label, command, icon=None):
    item_path = menu_root + r"\shell" + "\\" + name
    create_key(item_path)
    set_value(item_path, "MUIVerb", label)
    # MultiSelectModel = "Document" makes Explorer launch one verb instance per
    # selected item with %1 expanded to that single path. The follower processes
    # are coordinated by zap.exe via the batch lock + paths_dir machinery in
    # main.rs::run_batch, which gives correct behaviour on huge selections
    # without overflowing a single command line. Switching to "Player" would let
    # us drop run_batch but at the cost of a single command-line length budget.
    set_value(item_path, "MultiSelectModel", "Document")
    if icon:
        set_value(item_path, "Icon", icon)

    command_path = item_path + r"\command"
    create_key(command_path)
    set_value(command_path, "", command)


def register_context_menu(menu_root, target_placeholder):
    create_key(menu_root)
    set_value(menu_root, "MUIVerb", "Zap")
    set_value(menu_root, "Icon", f'"{ICON_PATH}"')
    set_value(menu_root, "Position", "Bottom")
    set_value(menu_root, "SubCommands", "")
    set_value(menu_root, "MultiSelectModel", "Document")

    add_menu_item(
        menu_root,
        "delete-dialog",
        "Delete...",
        f'"{ZAPG_PATH}" --batch "{target_placeholder}"',
        f'"{ICON_PATH}"',
    )
    add_menu_item(
        menu_root,
        "recycle",
        "Move to Recycle Bin",
        f'"{ZAPW_PATH}" --batch --silent --yes --recycle "{target_placeholder}"',
        f'"{ICON_PATH}"',
    )
    add_menu_item(
        menu_root,
        "zap-delete",
        "Zap Delete",
        f'"{ZAPW_PATH}" --batch --silent --yes "{target_placeholder}"',
        f'"{ICON_PATH}"',
    )


try:
    set_dword(EXPLORER_ROOT, "MultipleInvokePromptMinimum", MAX_CONTEXT_MENU_SELECTIONS)

    for root in OLD_ROOTS:
        delete_tree(winreg.HKEY_CURRENT_USER, key_path(root))
        delete_tree(winreg.HKEY_CLASSES_ROOT, root, ignore_permission=True)

    register_context_menu(OBJECT_MENU_ROOT, "%1")
    register_context_menu(BACKGROUND_MENU_ROOT, "%V")
except OSError as err:
    print(f"Failed to register Zap context menu: {err}", file=sys.stderr)
    sys.exit(1)
