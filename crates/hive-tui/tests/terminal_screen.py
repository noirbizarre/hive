"""Small cursor-addressed screen used to assert terminal output, rather than raw diff fragments."""

import codecs
import re
import unicodedata


class Screen:
    def __init__(self):
        self.cells = {}
        self.row = 0
        self.column = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")

    def feed(self, chunk):
        self.pending += self.decoder.decode(chunk)
        while self.pending:
            if self.pending.startswith("\x1b["):
                match = re.match(r"\x1b\[([0-9;?]*)([@-~])", self.pending)
                if match is None:
                    return
                parameters, command = match.groups()
                self.pending = self.pending[match.end():]
                if parameters.startswith("?"):
                    if parameters == "?1049" and command == "h":
                        self.cells.clear()
                    continue
                values = [int(value or 0) for value in parameters.split(";")]
                first = values[0] or 1
                if command in "Hf":
                    self.row = first - 1
                    self.column = (values[1] or 1) - 1 if len(values) > 1 else 0
                elif command == "G":
                    self.column = first - 1
                elif command in "ABCD":
                    self.row += {"A": -first, "B": first}.get(command, 0)
                    self.column += {"C": first, "D": -first}.get(command, 0)
                elif command == "J" and values[0] in (2, 3):
                    self.cells.clear()
                elif command == "K":
                    self.cells = {position: text for position, text in self.cells.items()
                                  if position[0] != self.row or position[1] < self.column}
                continue
            if self.pending == "\x1b":
                return
            char, self.pending = self.pending[0], self.pending[1:]
            if char == "\r":
                self.column = 0
            elif char == "\n":
                self.row += 1
            elif not char.isprintable():
                continue
            elif not unicodedata.combining(char):
                self.cells[self.row, self.column] = char
                width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
                self.column += width

    def contains(self, text):
        for row in {row for row, _ in self.cells}:
            columns = [column for candidate, column in self.cells if candidate == row]
            line = "".join(self.cells.get((row, column), " ") for column in range(max(columns) + 1))
            if text in line:
                return True
        return False
