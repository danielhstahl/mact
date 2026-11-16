"""Module docstring."""
import os


class Runner:
    limit = 10

    def run(self, items, flag=True):
        total = 0
        for item in items:
            if item > 0 and flag:
                total += item
            elif item < 0 or not flag:
                total -= 1
            else:
                total += 1
        try:
            average = total / len(items) if items else 0
        except ZeroDivisionError:
            average = 0
        finally:
            os.sync()
        names = [n.strip() for n in os.listdir(".") if n]
        match total:
            case 0:
                total = 1
            case n if n > 0:
                total = n
            case _:
                total = -1
        (found := total)
        return f"{average:.1f}/{names} {found}"


def main() -> int:
    return Runner().run([1, 2], flag=False)
