#ifndef MATH_UTIL_H
#define MATH_UTIL_H

#define DOUBLE_IT(x) ((x) * 2)

struct Point { int x; int y; };
union Value { int i; float f; };
enum Color { RED, GREEN, BLUE };
typedef struct Point PointAlias;
typedef int (*BinaryOp)(int, int);

int add_numbers(int a, int b);
int scale_value(int v, int factor);
int apply_op(BinaryOp op, int a, int b);
int point_sum(const struct Point *p);
int enum_width(enum Color c);

#endif
