import re, sys

def to_pos(s):
    m = re.match(r'(-?)(\d)\.(\d+)e(-?)(\d+)$', s)
    if not m:
        return None
    sign, d0, rest, e = m.group(1), m.group(2), m.group(3), int(m.group(5))
    digits = (d0 + rest).rstrip('0') or '0'
    if e < -4 or e >= 17:
        frac = digits[1:] if len(digits) > 1 else '0'
        return sign + digits[0] + '.' + frac + 'e' + ('+' if e >= 0 else '-') + ('%02d' % abs(e))
    if e < 0:
        return sign + '0.' + digits
    if len(digits) <= e + 1:
        return sign + digits + '0' * (e + 1 - len(digits)) + '.0'
    return sign + digits[:e + 1] + '.' + digits[e + 1:]

rust = [l.strip() for l in open('rust_vals.txt') if l.strip()]
sql = [l.strip() for l in open('sql_vals.txt') if l.strip()]
print('counts', len(rust), len(sql))
mis = 0
ex = []
for r, s in zip(rust, sql):
    p = to_pos(r)
    if p != s:
        mis += 1
        if len(ex) < 15:
            ex.append((r, str(p), s))
for r, p, s in ex:
    print('MISMATCH rust=%-27s -> %-27s sqlite=%s' % (r, p, s))
print('total', len(rust), 'mismatches', mis)
