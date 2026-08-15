#ifndef WIDGET_H
#define WIDGET_H

#include <string>

namespace app {
namespace ui {

/// Abstract base: exercises virtual dispatch and an out-of-line method.
class Shape {
public:
    virtual ~Shape() = default;
    virtual int area() const = 0;
    int describe() const;
};

class Widget : public Shape {
public:
    explicit Widget(int size);
    ~Widget();

    int area() const override;   // out-of-line override in the .cpp
    int draw() const;            // out-of-line, the #698 regression shape
    int inlineSize() const { return size_; }

    // Overload set: same name, different arity.
    int resize(int by);
    int resize(int w, int h);

    Widget& operator+=(int by);

    static int instances();

private:
    int size_;
    static int count_;
};

/// Function template.
template <typename T>
T twice(T v) {
    return v + v;
}

/// Class template with an out-of-class member definition.
template <typename T>
class Box {
public:
    explicit Box(T v) : value_(v) {}
    T unwrap() const;
private:
    T value_;
};

template <typename T>
T Box<T>::unwrap() const {
    return value_;
}

int build_default();
std::string label_of(const Shape &s);

}  // namespace ui
}  // namespace app

#endif
