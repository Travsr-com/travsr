import os


class Greeter:
    def __init__(self, name):
        self.name = name

    def greet(self):
        return f"hello {self.name}"


def main():
    print(Greeter(os.getcwd()).greet())
