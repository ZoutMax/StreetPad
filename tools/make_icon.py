"""Generate res/streetpad.ico: blue rounded square, white map pin with a D-pad cross."""
import math, struct, zlib, sys

def render(n):
    ss = 4; N = n * ss
    px = bytearray(n * n * 4)
    def inside(x, y):  # x,y in 0..1, returns (r,g,b,a) or None
        # rounded square background
        r = 0.18; cx = min(max(x, r), 1 - r); cy = min(max(y, r), 1 - r)
        if (x - cx) ** 2 + (y - cy) ** 2 > r * r: return None
        t = y
        bg = (int(25 + 20 * t), int(118 - 40 * t), int(210 - 60 * t))
        # map pin: circle head + triangle tail
        hx, hy, hr = 0.5, 0.40, 0.27
        d = math.hypot(x - hx, y - hy)
        pin = d <= hr
        if not pin and 0.40 <= y <= 0.88:
            w = hr * 0.92 * (0.88 - y) / (0.88 - 0.40)
            pin = abs(x - hx) <= w and y >= hy
        if pin:
            # D-pad cross cut-out inside pin head
            a, b = 0.06, 0.18
            if d <= 0.035: return (255, 255, 255, 255)   # D-pad centre pivot
            if (abs(x - hx) <= a and abs(y - hy) <= b) or (abs(y - hy) <= a and abs(x - hx) <= b):
                return bg + (255,)
            return (255, 255, 255, 255)
        return bg + (255,)
    for j in range(n):
        for i in range(n):
            acc = [0, 0, 0, 0]
            for sj in range(ss):
                for si in range(ss):
                    c = inside((i * ss + si + .5) / N, (j * ss + sj + .5) / N)
                    if c:
                        for k in range(3): acc[k] += c[k] * c[3]
                        acc[3] += c[3]
            o = (j * n + i) * 4
            if acc[3]:
                px[o:o + 3] = bytes(int(acc[k] / acc[3]) for k in range(3))
                px[o + 3] = int(acc[3] / (ss * ss))
    return px

def png(n, px):
    raw = b''.join(b'\x00' + bytes(px[j * n * 4:(j + 1) * n * 4]) for j in range(n))
    def chunk(t, d): return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d) & 0xffffffff)
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', n, n, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b'')

sizes = [16, 24, 32, 48, 64, 128, 256]
imgs = [png(s, render(s)) for s in sizes]
out = struct.pack('<HHH', 0, 1, len(sizes)); off = 6 + 16 * len(sizes)
for s, d in zip(sizes, imgs):
    out += struct.pack('<BBBBHHII', s % 256, s % 256, 0, 0, 1, 32, len(d), off); off += len(d)
open(sys.argv[1], 'wb').write(out + b''.join(imgs))
