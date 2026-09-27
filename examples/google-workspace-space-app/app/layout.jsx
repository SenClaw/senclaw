import 'antd/dist/reset.css';

export const metadata = {
  title: 'Google Workspace Space App'
};

export default function RootLayout({ children }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
