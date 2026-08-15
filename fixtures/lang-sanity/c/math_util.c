#include "math_util.h"

/* Static: file-local linkage, still callable within this translation unit. */
static int clamp_low(int v) {
    return v < 0 ? 0 : v;
}

int add_numbers(int a, int b) {
    return clamp_low(a + b);
}

int scale_value(int v, int factor) {
    return add_numbers(v, v) * factor;
}

/* Called through a function pointer, never by name. */
int apply_op(BinaryOp op, int a, int b) {
    return op(a, b);
}

int point_sum(const struct Point *p) {
    return p->x + p->y;
}

int enum_width(enum Color c) {
    return c == RED ? 1 : DOUBLE_IT(2);
}
