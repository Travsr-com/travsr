#include <string>

namespace geometry {

class Point {
public:
    Point(int x, int y) : x_(x), y_(y) {}
    int x() const { return x_; }

private:
    int x_;
    int y_;
};

template <typename T>
T identity(T value) {
    return value;
}

}  // namespace geometry
