import socket

def send_message(host, port, message):
    # 创建套接字对象
    client_socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    
    try:
        # 连接到服务器
        client_socket.connect((host, port))
        
        # 发送消息
        client_socket.sendall(message.encode())
        
        # 接收服务器响应
        response = client_socket.recv(1024)
        print("收到服务器的响应：", response.decode())
        
    except ConnectionRefusedError:
        print("无法连接到服务器！")
        
    finally:
        # 关闭连接
        client_socket.close()

if __name__ == "__main__":
    host = "0.0.0.0"  # 服务器主机地址
    port = 8081         # 服务器端口号
    
    while True:
        message = input("请输入要发送的消息（输入 q 退出）：")
        if message == "q":
            break
        
        send_message(host, port, message)
