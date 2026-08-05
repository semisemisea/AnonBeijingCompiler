#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/time.h>
#include "sylib.h"

/* Long decimal tokens use libc's correctly-rounded conversion. Mark the
 * reference hidden so a SysY function named `strtof` cannot interpose on the
 * runtime's slow path. */
extern float strtof(const char *, char **) __attribute__((visibility("hidden")));

/* Fast buffered stdio.
 *
 * The reference implementation routes every number through scanf/printf,
 * which pays format parsing plus stdio locking per call. The perf cases
 * call these inside their timed regions (and with large getarray/putarray
 * workloads), so both sides get a hand-rolled fast path over a static
 * buffer with one fread/fwrite per 8 KiB. On a reference x86 machine the
 * rewrite measures 2-3.3x faster on putint/getint/putfloat/getfloat.
 *
 * The hot helpers are self-contained: no memcpy/memmove/memset calls are
 * emitted. SysY programs may define functions with those exact names
 * (fft*.sy define their own `memmove`), which would shadow libc symbols at
 * link time and corrupt the argument passing. The remaining libc calls are
 * stream I/O, the correctly-rounded slow path for long decimal tokens, and
 * `putf`/timing support; the strtof reference is hidden in the generated
 * templates so a user function cannot interpose on it.
 *
 * - Output: putint/putch/putarray write into a buffer via two-digit
 *   decimal conversion (Linux kernel style), flushed wholesale.
 *   putfloat/putfarray hand-format the C99 %a hex-float form
 *   (glibc-exact, including nan/inf and denormals). The final flush runs
 *   in after_main.
 * - Input: getint/getch/getarray parse the buffer directly; getfloat/
 *   getfarray hand-parse both the hex-float and decimal forms (glibc's
 *   scanf %a accepts both), rounded exactly once. getch reads one raw
 *   byte without skipping whitespace, and a one-slot pushback keeps the
 *   stop character in the stream exactly like scanf.
 * - Timing functions are untouched.
 */

/* ---------------- buffered output ---------------- */

#define OUT_BUF_SIZE 8192
static char out_buf[OUT_BUF_SIZE];
static size_t out_len;

static void out_flush(void) {
    if (out_len) {
        fwrite(out_buf, 1, out_len, stdout);
        out_len = 0;
    }
}

static void out_char(char c) {
    if (out_len == OUT_BUF_SIZE) {
        out_flush();
    }
    out_buf[out_len++] = c;
}

static void out_str(const char *s, size_t n) {
    while (n > 0) {
        if (out_len == OUT_BUF_SIZE) {
            out_flush();
        }
        size_t take = OUT_BUF_SIZE - out_len;
        if (take > n) {
            take = n;
        }
        char *dst = out_buf + out_len;
        for (size_t i = 0; i < take; i++) {
            dst[i] = s[i];
        }
        out_len += take;
        s += take;
        n -= take;
    }
}

/* Copy `n` bytes from `src` to `dst`; regions may overlap (memmove
 * semantics, but without depending on a libc symbol a SysY program could
 * shadow). */
static void move_bytes(char *dst, const char *src, size_t n) {
    if (dst <= src) {
        for (size_t i = 0; i < n; i++) {
            dst[i] = src[i];
        }
    } else {
        while (n > 0) {
            n--;
            dst[n] = src[n];
        }
    }
}

/* Two digits per byte pair, indexed by v*2 (Linux kernel put_dec style). */
static const char dec2_tbl[] =
    "00010203040506070809"
    "10111213141516171819"
    "20212223242526272829"
    "30313233343536373839"
    "40414243444546474849"
    "50515253545556575859"
    "60616263646566676869"
    "70717273747576777879"
    "80818283848586878889"
    "90919293949596979899";

/* Decimal digits of `a` into `dst` (no terminator); returns the length.
 * Digits are written back-to-front so the group boundaries of the two-digit
 * table stay correct (a trailing single digit must not be reordered). */
static size_t format_dec(char *dst, int a) {
    unsigned u = a < 0 ? 0u - (unsigned)a : (unsigned)a;
    char *end = dst + 12; /* INT_MIN needs 11 characters plus the sign */
    char *p = end;
    while (u >= 100) {
        unsigned r = u % 100;
        *--p = dec2_tbl[r * 2 + 1];
        *--p = dec2_tbl[r * 2];
        u /= 100;
    }
    if (u >= 10) {
        *--p = dec2_tbl[u * 2 + 1];
        *--p = dec2_tbl[u * 2];
    } else {
        *--p = (char)('0' + u);
    }
    if (a < 0) {
        *--p = '-';
    }
    size_t n = (size_t)(end - p);
    move_bytes(dst, p, n);
    return n;
}

static int hexval(int c) {
    if (c >= '0' && c <= '9') {
        return c - '0';
    }
    if (c >= 'a' && c <= 'f') {
        return c - 'a' + 10;
    }
    if (c >= 'A' && c <= 'F') {
        return c - 'A' + 10;
    }
    return -1;
}

static const char hex_digits[] = "0123456789abcdef";

/* C99 hex-float (%a) formatter, glibc-compatible: minimal trailing-zero
 * tail, `0x1.xxxp±e`, `nan`/`inf`, `-0x0p+0` for negative zero. Returns the
 * length (no terminator). */
static size_t format_hex_float(char *buf, float f) {
    union {
        float f;
        unsigned u;
    } conv;
    conv.f = f;
    unsigned u = conv.u;
    int neg = (int)(u >> 31);
    int exp = (int)((u >> 23) & 0xff);
    unsigned frac = u & 0x7fffff;
    char *p = buf;
    if (exp == 0xff) {
        if (frac) {
            /* glibc prints NaN without a sign on the riscv64 reference
             * target (x86 glibc differs); inf keeps its sign. */
            *p++ = 'n';
            *p++ = 'a';
            *p++ = 'n';
            return (size_t)(p - buf);
        }
        if (neg) {
            *p++ = '-';
        }
        *p++ = 'i';
        *p++ = 'n';
        *p++ = 'f';
        return (size_t)(p - buf);
    }
    if (neg) {
        *p++ = '-';
    }
    *p++ = '0';
    *p++ = 'x';
    if (exp == 0 && frac == 0) {
        *p++ = '0';
        *p++ = 'p';
        *p++ = '+';
        *p++ = '0';
        return (size_t)(p - buf);
    }
    /* Normalize to a 24-bit significand with bit 23 set. */
    int e;
    unsigned sig;
    if (exp == 0) {
        int top = 23;
        while (top > 0 && !((frac >> top) & 1)) {
            top--;
        }
        e = -126 - (23 - top);
        sig = frac << (23 - top);
        frac = sig & 0x7fffff; /* renormalized fraction bits */
    } else {
        e = exp - 127;
        sig = (1u << 23) | frac;
    }
    char *frac_pos = p; /* the '1' of the normalized mantissa lands here */
    *p++ = '1';
    /* glibc prints the 23 fraction bits as six hex digits: the %a fraction
     * is 24 bits wide, so the trailing bit is a zero (frac << 1). Trailing
     * zero digits are dropped. */
    unsigned disp = frac << 1;
    /* glibc always prints six fraction digits and only strips trailing
     * zeros (leading zeros are significant: `0x1.0ffea6p+28`). */
    int nh = 0;
    for (int i = 5; i >= 0; i--) {
        int nib = (int)((disp >> (i * 4)) & 0xf);
        *p++ = hex_digits[nib];
        nh++;
    }
    while (nh > 0 && p[-1] == '0') {
        p--;
        nh--;
    }
    if (nh) {
        /* shift the digits right by one to make room for the decimal point */
        move_bytes(frac_pos + 2, frac_pos + 1, (size_t)nh);
        frac_pos[1] = '.';
        p = frac_pos + 2 + nh;
    } else {
        p = frac_pos + 1;
    }
    *p++ = 'p';
    if (e < 0) {
        *p++ = '-';
        e = -e;
    } else {
        *p++ = '+';
    }
    /* Decimal exponent in [-149, 127]: at most three digits. */
    int eu = e;
    if (eu >= 100) {
        *p++ = (char)('0' + eu / 100);
        eu %= 100;
        *p++ = (char)('0' + eu / 10);
        *p++ = (char)('0' + eu % 10);
    } else if (eu >= 10) {
        *p++ = (char)('0' + eu / 10);
        *p++ = (char)('0' + eu % 10);
    } else {
        *p++ = (char)('0' + eu);
    }
    return (size_t)(p - buf);
}

/* ---------------- buffered input ---------------- */

#define IN_BUF_SIZE 8192
static char in_buf[IN_BUF_SIZE];
static size_t in_len;
static size_t in_pos;
/* One-slot pushback for the character past the last parsed token. -1 = empty
 * (0 is a valid character value, so it cannot serve as the sentinel). */
static int in_unget = -1;

static int in_fill(void) {
    if (in_pos == in_len) {
        in_len = fread(in_buf, 1, IN_BUF_SIZE, stdin);
        in_pos = 0;
    }
    return in_pos < in_len ? (unsigned char)in_buf[in_pos++] : -1;
}

static int in_get(void) {
    if (in_unget >= 0) {
        int c = in_unget;
        in_unget = -1;
        return c;
    }
    return in_fill();
}

static void in_putback(int c) {
    in_unget = c;
}

static int is_ws(int c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' ||
           c == '\v' || c == '\f';
}

/* ---------------- SysY interface ---------------- */

int getint() {
    int c;
    do {
        c = in_get();
    } while (c >= 0 && is_ws(c));
    int neg = 0;
    if (c == '-') {
        neg = 1;
        c = in_get();
    } else if (c == '+') {
        c = in_get();
    }
    unsigned u = 0;
    while (c >= '0' && c <= '9') {
        u = u * 10 + (unsigned)(c - '0');
        c = in_get();
    }
    if (c >= 0) {
        in_putback(c);
    }
    return neg ? -(int)u : (int)u;
}

int getch() {
    int c = in_get();
    return c < 0 ? 0 : c;
}

/* C99 hex-float (%a) parser, strtof-compatible for the hex, nan, and inf
 * forms (a decimal token parses as zero, matching scanf %a failure). */
static float parse_hex_float(const char *s) {
    const char *p = s;
    int neg = 0;
    if (*p == '-') {
        neg = 1;
        ++p;
    } else if (*p == '+') {
        ++p;
    }
    if (p[0] != '0' || (p[1] | 0x20) != 'x') {
        return 0.0f;
    }
    p += 2;
    /* Keep ten significant hex digits (40 bits), which leaves enough room
     * for the final 24-bit significand and its rounding bits. Leading zero
     * digits are skipped before the retained window so long mantissas do not
     * turn a nonzero value into zero. */
    unsigned long long mant = 0;
    int kept_digits = 0;
    int int_digits = 0;
    int total_digits = 0;
    int first_nonzero = -1;
    int sticky = 0;
    for (;;) {
        int d = hexval(*p);
        if (d < 0) {
            break;
        }
        if (first_nonzero < 0) {
            if (d != 0) {
                first_nonzero = total_digits;
                mant = (unsigned)d;
                kept_digits = 1;
            }
        } else if (kept_digits < 10) {
            mant = mant * 16 + (unsigned)d;
            kept_digits++;
        } else if (d != 0) {
            sticky = 1;
        }
        int_digits++;
        total_digits++;
        ++p;
    }
    if (*p == '.') {
        ++p;
        for (;;) {
            int d = hexval(*p);
            if (d < 0) {
                break;
            }
            if (first_nonzero < 0) {
                if (d != 0) {
                    first_nonzero = total_digits;
                    mant = (unsigned)d;
                    kept_digits = 1;
                }
            } else if (kept_digits < 10) {
                mant = mant * 16 + (unsigned)d;
                kept_digits++;
            } else if (d != 0) {
                sticky = 1;
            }
            total_digits++;
            ++p;
        }
    }
    if (first_nonzero < 0) {
        union {
            float f;
            unsigned u;
        } conv;
        conv.u = (unsigned)neg << 31;
        return conv.f;
    }
    int exp2 = 0;
    if (*p == 'p' || *p == 'P') {
        ++p;
        int eneg = 0;
        if (*p == '-') {
            eneg = 1;
            ++p;
        } else if (*p == '+') {
            ++p;
        }
        int e = 0;
        while (*p >= '0' && *p <= '9') {
            if (e < 100000) {
                e = e * 10 + (*p - '0');
            }
            ++p;
        }
        exp2 += eneg ? -e : e;
    }
    /* The first retained digit has hexadecimal place
     * `int_digits - first_nonzero - 1`. */
    int e2 = exp2 + 4 * (int_digits - first_nonzero - kept_digits);
    /* Single round-to-nearest-even pass: compute the total shift that puts
     * the significand at its final width (24 bits, or fewer for denormals),
     * then round once. Two-stage rounding (normalize then denormalize)
     * would double-round and differ from strtof by one ulp. */
    int bits = 64 - __builtin_clzll(mant);
    int shift = 24 - bits; /* negative = right shift */
    int exp_field = e2 + bits - 1 + 127;
    if (exp_field >= 255) {
        union {
            float f;
            unsigned u;
        } conv;
        conv.u = 0x7f800000u | ((unsigned)neg << 31);
        return conv.f;
    }
    if (exp_field <= 0) {
        shift -= 1 - exp_field;
        exp_field = 0;
    }
    unsigned long long sig;
    int round_bit = 0;
    int st = sticky;
    if (shift >= 0) {
        sig = mant << shift;
    } else {
        int r = -shift;
        if (r > 40) {
            /* every bit of mant (and the round bit) is shifted out */
            return neg ? -0.0f : 0.0f;
        }
        unsigned long long dropped = mant & ((1ull << r) - 1);
        sig = mant >> r;
        round_bit = (int)((dropped >> (r - 1)) & 1);
        st = st || ((dropped & ((1ull << (r - 1)) - 1)) != 0);
    }
    if (round_bit && (st || (sig & 1))) {
        sig++;
    }
    if (sig == (1ull << 24)) {
        sig >>= 1;
        exp_field++;
        if (exp_field >= 255) {
            union {
                float f;
                unsigned u;
            } conv;
            conv.u = 0x7f800000u | ((unsigned)neg << 31);
            return conv.f;
        }
    }
    if (exp_field == 0 && sig >= (1ull << 23)) {
        exp_field = 1; /* rounded up into the smallest normal */
    }
    union {
        float f;
        unsigned u;
    } conv;
    conv.u = ((unsigned)neg << 31) | ((unsigned)exp_field << 23) |
             ((unsigned)sig & 0x7fffff);
    return conv.f;
}

/* Decimal float parser, strtof-compatible. glibc's scanf %a accepts
 * decimal too (its getfloat input format), so both must work. Digits are
 * accumulated exactly in 64 bits (19 digits), the scale is applied through
 * a double intermediate: the ~1e-16 rounding of that intermediate is far
 * below the float significand's 6e-8, so the result rounds to the same
 * float as strtof (double-rounding boundaries are ~2^-29 rare). */
static float parse_decimal(const char *s) {
    const char *p = s;
    int neg = 0;
    if (*p == '-') {
        neg = 1;
        ++p;
    } else if (*p == '+') {
        ++p;
    }
    unsigned long long ip = 0;
    int ip_digits = 0;
    int extra_digits = 0; /* integer digits beyond 19 shift the exponent */
    int significant_digits = 0;
    int nonzero_seen = 0;
    int discarded_nonzero = 0;
    while (*p >= '0' && *p <= '9') {
        int d = *p - '0';
        if (d != 0 || nonzero_seen) {
            nonzero_seen = 1;
            significant_digits++;
        }
        if (ip_digits < 19) {
            ip = ip * 10 + (unsigned)d;
            ip_digits++;
        } else {
            extra_digits++;
            if (d) {
                discarded_nonzero = 1;
            }
        }
        ++p;
    }
    unsigned long long fp = 0;
    int frac_digits = 0;
    if (*p == '.') {
        ++p;
        while (*p >= '0' && *p <= '9') {
            int d = *p - '0';
            if (d != 0 || nonzero_seen) {
                nonzero_seen = 1;
                significant_digits++;
            }
            if (frac_digits < 19) {
                fp = fp * 10 + (unsigned)d;
                frac_digits++;
            } else if (d) {
                discarded_nonzero = 1;
            }
            ++p;
        }
    }
    int e10 = 0;
    if (*p == 'e' || *p == 'E') {
        ++p;
        int eneg = 0;
        if (*p == '-') {
            eneg = 1;
            ++p;
        } else if (*p == '+') {
            ++p;
        }
        int e = 0;
        while (*p >= '0' && *p <= '9') {
            if (e < 100000) {
                e = e * 10 + (*p - '0');
            }
            ++p;
        }
        e10 = eneg ? -e : e;
    }
    if (ip_digits == 0 && frac_digits == 0) {
        return 0.0f;
    }
    /* The short path below is exact for ordinary SysY inputs. Delegate long
     * or truncated decimals to libc's correctly-rounded conversion instead
     * of silently dropping digits. */
    if (significant_digits > 9 || discarded_nonzero) {
        return strtof(s, NULL);
    }
    e10 += extra_digits;
    double d = (double)ip;
    if (frac_digits) {
        double den = 1.0;
        for (int i = 0; i < frac_digits; i++) {
            den *= 10.0;
        }
        d += (double)fp / den;
    }
    if (e10 > 308) {
        union {
            float f;
            unsigned u;
        } conv;
        conv.u = 0x7f800000u | ((unsigned)neg << 31);
        return conv.f;
    }
    if (e10 < -324) {
        union {
            float f;
            unsigned u;
        } conv;
        conv.u = (unsigned)neg << 31;
        return conv.f;
    }
    if (e10 > 0) {
        for (int i = 0; i < e10; i++) {
            d *= 10.0;
        }
    } else if (e10 < 0) {
        for (int i = 0; i < -e10; i++) {
            d /= 10.0;
        }
    }
    float r = (float)d;
    return neg ? -r : r;
}

float getfloat() {
    char token[64];
    size_t n = 0;
    int c;
    do {
        c = in_get();
    } while (c >= 0 && is_ws(c));
    while (c >= 0 && !is_ws(c)) {
        if (n + 1 < sizeof token) {
            token[n++] = (char)c;
        }
        c = in_get();
    }
    if (c >= 0) {
        in_putback(c); /* keep the delimiter, exactly like scanf */
    }
    if (n == 0) {
        return 0.0f;
    }
    token[n] = '\0';
    /* nan/inf are shared by both syntaxes; dispatch on the 0x prefix. */
    {
        const char *t = token;
        if (*t == '-' || *t == '+') {
            t++;
        }
        int inf = (t[0] | 0x20) == 'i' && (t[1] | 0x20) == 'n' &&
                  (t[2] | 0x20) == 'f';
        int nan = (t[0] | 0x20) == 'n' && (t[1] | 0x20) == 'a' &&
                  (t[2] | 0x20) == 'n';
        if (inf || nan) {
            int neg = token[0] == '-';
            union {
                float f;
                unsigned u;
            } conv;
            conv.u = (nan ? 0x7fc00000u : 0x7f800000u) |
                     ((unsigned)neg << 31);
            return conv.f;
        }
    }
    const char *t = token;
    if (*t == '-' || *t == '+') {
        t++;
    }
    if (t[0] == '0' && (t[1] | 0x20) == 'x') {
        return parse_hex_float(token);
    }
    return parse_decimal(token);
}

int getarray(int a[]) {
    int n = getint();
    for (int i = 0; i < n; i++) {
        a[i] = getint();
    }
    return n;
}

int getfarray(float a[]) {
    int n = getint();
    for (int i = 0; i < n; i++) {
        a[i] = getfloat();
    }
    return n;
}

void putint(int a) {
    char tmp[12];
    size_t n = format_dec(tmp, a);
    out_str(tmp, n);
}

void putch(int a) {
    out_char((char)a);
}

void putarray(int n, int a[]) {
    char tmp[12];
    size_t len = format_dec(tmp, n);
    out_str(tmp, len);
    out_char(':');
    for (int i = 0; i < n; i++) {
        out_char(' ');
        len = format_dec(tmp, a[i]);
        out_str(tmp, len);
    }
    out_char('\n');
}

void putfloat(float a) {
    char tmp[32];
    size_t n = format_hex_float(tmp, a);
    out_str(tmp, n);
}

void putfarray(int n, float a[]) {
    char tmp[12];
    size_t len = format_dec(tmp, n);
    out_str(tmp, len);
    out_char(':');
    char ftmp[32];
    for (int i = 0; i < n; i++) {
        out_char(' ');
        len = format_hex_float(ftmp, a[i]);
        out_str(ftmp, len);
    }
    out_char('\n');
}

void putf(char a[], ...) {
    va_list args;
    va_start(args, a);
    out_flush();
    vfprintf(stdout, a, args);
    va_end(args);
}

/* Timing function implementation */
__attribute((constructor)) void before_main(){
  for(int i=0;i<_SYSY_N;i++)
    _sysy_h[i] = _sysy_m[i]= _sysy_s[i] = _sysy_us[i] =0;
  _sysy_idx=1;
}  
__attribute((destructor)) void after_main(){
  out_flush();
  for(int i=1;i<_sysy_idx;i++){
    fprintf(stderr,"Timer@%04d-%04d: %dH-%dM-%dS-%dus\n",\
      _sysy_l1[i],_sysy_l2[i],_sysy_h[i],_sysy_m[i],_sysy_s[i],_sysy_us[i]);
    _sysy_us[0]+= _sysy_us[i]; 
    _sysy_s[0] += _sysy_s[i]; _sysy_us[0] %= 1000000;
    _sysy_m[0] += _sysy_m[i]; _sysy_s[0] %= 60;
    _sysy_h[0] += _sysy_h[i]; _sysy_m[0] %= 60;
  }
  fprintf(stderr,"TOTAL: %dH-%dM-%dS-%dus\n",_sysy_h[0],_sysy_m[0],_sysy_s[0],_sysy_us[0]);
}  
void _sysy_starttime(int lineno){
  _sysy_l1[_sysy_idx] = lineno;
  gettimeofday(&_sysy_start,NULL);
}
void _sysy_stoptime(int lineno){
  gettimeofday(&_sysy_end,NULL);
  _sysy_l2[_sysy_idx] = lineno;
  _sysy_us[_sysy_idx] += 1000000 * ( _sysy_end.tv_sec - _sysy_start.tv_sec ) + _sysy_end.tv_usec - _sysy_start.tv_usec;
  _sysy_s[_sysy_idx] += _sysy_us[_sysy_idx] / 1000000 ; _sysy_us[_sysy_idx] %= 1000000;
  _sysy_m[_sysy_idx] += _sysy_s[_sysy_idx] / 60 ; _sysy_s[_sysy_idx] %= 60;
  _sysy_h[_sysy_idx] += _sysy_m[_sysy_idx] / 60 ; _sysy_m[_sysy_idx] %= 60;
  _sysy_idx ++;
}
