import sys
import winreg


CLASSES_ROOT = r"Software\Classes"
ROOTS = [
    r"Directory\shell\turbo-delete",
    r"Directory\Background\shell\turbo-delete",
    r"*\shell\turbo-delete",
    r"AllFilesystemObjects\shell\turbo-delete",
    r"Directory\shell\zap",
    r"Directory\Background\shell\zap",
    r"*\shell\zap",
    r"AllFilesystemObjects\shell\zap",
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


try:
    for root in ROOTS:
        delete_tree(winreg.HKEY_CURRENT_USER, key_path(root))
        delete_tree(winreg.HKEY_CLASSES_ROOT, root, ignore_permission=True)
except OSError as err:
    print(f"Failed to unregister Zap context menu: {err}", file=sys.stderr)
    sys.exit(1)
