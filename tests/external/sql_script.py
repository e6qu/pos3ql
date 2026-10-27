"""Lexical SQL boundaries shared by the PostgreSQL comparison runners."""

import re
from typing import NamedTuple


class CopyInputSpan(NamedTuple):
    command_end_line: int
    data_end_line: int


class SqlScript(NamedTuple):
    statements: tuple
    copy_inputs: tuple[CopyInputSpan, ...]


def split_sql(source):
    """Split SQL and record COPY input spans without parsing payload as SQL."""
    statements = []
    copy_inputs = []
    words = []
    parentheses = 0
    escaped_string = False
    buffer = []
    state = "normal"
    dollar = ""
    block_depth = 0
    line = 1
    first_line = 1
    has_code = False
    index = 0

    def starts_dollar(offset):
        if offset and (source[offset - 1].isalnum() or source[offset - 1] in "_$"):
            return None
        match = re.match(r"\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$", source[offset:])
        return match.group(0) if match else None

    while index < len(source):
        char = source[index]
        following = source[index + 1] if index + 1 < len(source) else ""
        if state == "normal":
            if char == "'":
                escaped_string = (index > 0 and source[index - 1] in "eE"
                                  and (index < 2 or not (source[index - 2].isalnum()
                                                        or source[index - 2] in "_$")))
                state = "single"
                has_code = True
                buffer.append(char)
            elif char == '"':
                if parentheses == 0:
                    words.append('"identifier"')
                state = "double"
                has_code = True
                buffer.append(char)
            elif char == "-" and following == "-":
                state = "line_comment"
                buffer.extend((char, following))
                index += 1
            elif char == "/" and following == "*":
                state = "block_comment"
                block_depth = 1
                buffer.extend((char, following))
                index += 1
            elif char == "$" and (tag := starts_dollar(index)) is not None:
                state = "dollar"
                dollar = tag
                has_code = True
                buffer.append(tag)
                index += len(tag) - 1
            elif char == ";":
                buffer.append(char)
                statement = "".join(buffer).strip()
                if has_code:
                    copy_data = None
                    if (words and words[0] == "COPY"
                            and any(pair == ["FROM", "STDIN"]
                                    for pair in (words[i:i + 2] for i in range(len(words) - 1)))):
                        command_end_line = line
                        header_end = source.find("\n", index + 1)
                        if header_end == -1:
                            raise ValueError(f"unterminated COPY data at line {first_line}")
                        remainder = source[index + 1:header_end].strip()
                        if remainder and not remainder.startswith("--"):
                            raise ValueError(f"COPY input must start on a new line at line {line}")
                        data_start = header_end + 1
                        terminator = re.search(r"(?m)^\\\.[ \t]*(?:\r?\n|$)", source[data_start:])
                        if terminator is None:
                            raise ValueError(f"unterminated COPY data at line {first_line}")
                        copy_data = source[data_start:data_start + terminator.start()]
                        marker_start = data_start + terminator.start()
                        copy_inputs.append(CopyInputSpan(
                            command_end_line,
                            line + source[index + 1:marker_start].count("\n"),
                        ))
                        consumed_end = data_start + terminator.end()
                        line += source[index + 1:consumed_end].count("\n")
                        index = consumed_end - 1
                    statements.append((first_line, statement, copy_data))
                buffer.clear()
                first_line = line
                has_code = False
                words.clear()
                parentheses = 0
            elif char.isalpha() or char == "_":
                end = index + 1
                while end < len(source) and (source[end].isalnum() or source[end] in "_$"):
                    end += 1
                word = source[index:end]
                if parentheses == 0:
                    words.append(word.upper())
                buffer.append(word)
                has_code = True
                index = end - 1
            else:
                if char == "(":
                    if parentheses == 0:
                        words.append("(")
                    parentheses += 1
                elif char == ")":
                    parentheses -= 1
                buffer.append(char)
                if not char.isspace():
                    has_code = True
        elif state == "single":
            buffer.append(char)
            if char == "\\" and escaped_string and following:
                buffer.append(following)
                if following == "\n":
                    line += 1
                index += 1
            elif char == "'":
                if following == "'":
                    buffer.append(following)
                    index += 1
                else:
                    state = "normal"
        elif state == "double":
            buffer.append(char)
            if char == '"':
                if following == '"':
                    buffer.append(following)
                    index += 1
                else:
                    state = "normal"
        elif state == "dollar":
            if source.startswith(dollar, index):
                buffer.append(dollar)
                index += len(dollar) - 1
                state = "normal"
            else:
                buffer.append(char)
        elif state == "line_comment":
            buffer.append(char)
            if char == "\n":
                state = "normal"
        else:
            buffer.append(char)
            if char == "/" and following == "*":
                buffer.append(following)
                index += 1
                block_depth += 1
            elif char == "*" and following == "/":
                buffer.append(following)
                index += 1
                block_depth -= 1
                if block_depth == 0:
                    state = "normal"
        if char == "\n":
            line += 1
            if not buffer or not "".join(buffer).strip():
                first_line = line
        index += 1

    if state not in {"normal", "line_comment"}:
        raise ValueError(f"unterminated {state} at end of SQL input")
    trailing = "".join(buffer).strip()
    if has_code:
        statements.append((first_line, trailing, None))
    return SqlScript(tuple(statements), tuple(copy_inputs))

