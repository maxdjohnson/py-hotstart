import curses

def main(stdscr):
    # Disable cursor and enable keypad
    curses.curs_set(0)
    stdscr.keypad(True)

    # Colors
    curses.start_color()
    curses.init_pair(1, curses.COLOR_CYAN, curses.COLOR_BLACK)

    # Initial UI state
    current_row = 0
    menu = ["Option 1", "Option 2", "Option 3", "Exit"]

    def draw_menu():
        stdscr.clear()
        h, w = stdscr.getmaxyx()
        for idx, row in enumerate(menu):
            x = w // 2 - len(row) // 2
            y = h // 2 - len(menu) // 2 + idx
            if idx == current_row:
                stdscr.attron(curses.color_pair(1))
                stdscr.addstr(y, x, row)
                stdscr.attroff(curses.color_pair(1))
            else:
                stdscr.addstr(y, x, row)
        stdscr.refresh()

    while True:
        draw_menu()
        key = stdscr.getch()

        # Navigation logic
        if key == curses.KEY_UP and current_row > 0:
            current_row -= 1
        elif key == curses.KEY_DOWN and current_row < len(menu) - 1:
            current_row += 1
        elif key == ord('\n'):  # Enter key
            if menu[current_row] == "Exit":
                break
            stdscr.clear()
            stdscr.addstr(0, 0, f"You selected '{menu[current_row]}'")
            stdscr.refresh()
            stdscr.getch()

if __name__ == "__main__":
    curses.wrapper(main)
